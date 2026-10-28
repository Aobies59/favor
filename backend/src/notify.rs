//! Notifications: stored per user inside the action's transaction, then
//! delivered as browser push (Web Push + VAPID) by a background sender.

use std::{sync::Arc, time::Duration};

use axum::{extract::State, Json};
use base64ct::{Base64UrlUnpadded, Encoding};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{postgres::PgListener, PgConnection, PgPool};
use web_push_native::{
    jwt_simple::algorithms::{ECDSAP256PublicKeyLike, ES256KeyPair},
    p256, Auth, WebPushBuilder,
};

use crate::{
    auth::Me,
    error::{AppError, AppResult},
    snapshot, AppState,
};

const CHANNEL: &str = "favor_push";

/// Stores a notification for each of `users`. Runs inside the caller's
/// transaction; Postgres wakes the push sender only once it commits.
pub async fn notify(
    c: &mut PgConnection,
    users: &[i32],
    kind: &str,
    title: String,
    body: String,
    link: &str,
) -> AppResult<()> {
    if users.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO notifications (user_id, kind, title, body, link)
         SELECT u, $2, $3, $4, $5 FROM unnest($1::int[]) AS u",
    )
    .bind(users)
    .bind(kind)
    .bind(title)
    .bind(body)
    .bind(link)
    .execute(&mut *c)
    .await?;
    sqlx::query("SELECT pg_notify($1, '')").bind(CHANNEL).execute(c).await?;
    Ok(())
}

// ── Push delivery ───────────────────────────────────────────────────────

pub struct Push {
    key: ES256KeyPair,
    /// Uncompressed public key, base64url: the browser's applicationServerKey.
    pub public_key: String,
    /// VAPID contact (`https://…` or `mailto:…`), required by push services.
    contact: String,
    http: reqwest::Client,
}

impl Push {
    /// Loads the VAPID key from the database, creating it on first start.
    pub async fn load(pool: &PgPool, contact: String) -> anyhow::Result<Self> {
        let stored: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = 'vapid_private'")
            .fetch_optional(pool)
            .await?;
        let key = match stored {
            Some((b64,)) => ES256KeyPair::from_bytes(&Base64UrlUnpadded::decode_vec(&b64)?)?,
            None => {
                let key = ES256KeyPair::generate();
                sqlx::query("INSERT INTO settings (key, value) VALUES ('vapid_private', $1) ON CONFLICT DO NOTHING")
                    .bind(Base64UrlUnpadded::encode_string(&key.to_bytes()))
                    .execute(pool)
                    .await?;
                tracing::info!("generated the push notification (VAPID) key");
                // Re-read in case another instance won the race.
                let (b64,): (String,) = sqlx::query_as("SELECT value FROM settings WHERE key = 'vapid_private'")
                    .fetch_one(pool)
                    .await?;
                ES256KeyPair::from_bytes(&Base64UrlUnpadded::decode_vec(&b64)?)?
            }
        };
        let public_key = Base64UrlUnpadded::encode_string(&key.public_key().public_key().to_bytes_uncompressed());
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
        Ok(Self { key, public_key, contact, http })
    }
}

#[derive(sqlx::FromRow)]
struct Pending {
    id: i64,
    user_id: i32,
    title: String,
    body: String,
    link: String,
}

#[derive(sqlx::FromRow)]
struct Subscription {
    endpoint: String,
    p256dh: String,
    auth: String,
}

/// Sends whatever hasn't been pushed yet whenever Postgres signals a commit,
/// and every 30 seconds as a fallback.
pub fn spawn_sender(pool: PgPool, push: Arc<Push>) {
    tokio::spawn(async move {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(mut l) => match l.listen(CHANNEL).await {
                Ok(()) => Some(l),
                Err(e) => {
                    tracing::warn!("push: LISTEN failed, polling only: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("push: no listener connection, polling only: {e}");
                None
            }
        };
        loop {
            if let Err(e) = deliver_pending(&pool, &push).await {
                tracing::error!("push delivery failed: {}", e.message);
            }
            match listener.as_mut() {
                Some(l) => {
                    if let Ok(Err(e)) = tokio::time::timeout(Duration::from_secs(30), l.recv()).await {
                        tracing::warn!("push: listener error ({e}), retrying");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
                None => tokio::time::sleep(Duration::from_secs(30)).await,
            }
        }
    });
}

async fn deliver_pending(pool: &PgPool, push: &Push) -> AppResult<()> {
    // Anything older than a day is no longer worth a buzz.
    sqlx::query("UPDATE notifications SET pushed_at = now() WHERE pushed_at IS NULL AND created_at < now() - interval '1 day'")
        .execute(pool)
        .await?;
    // Claim first, then send: a crash means a missed buzz, never a repeated one.
    let pending: Vec<Pending> = sqlx::query_as(
        "UPDATE notifications SET pushed_at = now()
         WHERE id IN (SELECT id FROM notifications WHERE pushed_at IS NULL ORDER BY id LIMIT 100)
         RETURNING id, user_id, title, body, link",
    )
    .fetch_all(pool)
    .await?;
    for n in pending {
        let subs: Vec<Subscription> =
            sqlx::query_as("SELECT endpoint, p256dh, auth FROM push_subscriptions WHERE user_id = $1")
                .bind(n.user_id)
                .fetch_all(pool)
                .await?;
        let payload = json!({ "title": n.title, "body": n.body, "link": n.link, "tag": format!("favor-{}", n.id) });
        for s in subs {
            match send(push, &s, payload.to_string()).await {
                Ok(SendResult::Sent) => {}
                Ok(SendResult::Gone) => {
                    tracing::info!("push: subscription expired, removing it");
                    sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = $1")
                        .bind(&s.endpoint)
                        .execute(pool)
                        .await?;
                }
                Err(e) => tracing::warn!("push: could not deliver notification {}: {e:#}", n.id),
            }
        }
    }
    Ok(())
}

enum SendResult {
    Sent,
    /// The browser dropped this subscription (404/410); forget it.
    Gone,
}

async fn send(push: &Push, s: &Subscription, payload: String) -> anyhow::Result<SendResult> {
    let builder = WebPushBuilder::new(
        s.endpoint.parse()?,
        p256::PublicKey::from_sec1_bytes(&Base64UrlUnpadded::decode_vec(&s.p256dh)?)?,
        Auth::clone_from_slice(&Base64UrlUnpadded::decode_vec(&s.auth)?),
    )
    .with_vapid(&push.key, &push.contact);
    let req = builder.build(payload)?;
    let res = push
        .http
        .post(req.uri().to_string())
        .headers(req.headers().clone())
        .body(req.into_body())
        .send()
        .await?;
    match res.status().as_u16() {
        200..=299 => Ok(SendResult::Sent),
        404 | 410 => Ok(SendResult::Gone),
        code => anyhow::bail!("push service answered {code}: {}", res.text().await.unwrap_or_default()),
    }
}

// ── API ─────────────────────────────────────────────────────────────────

/// The public VAPID key the browser needs to subscribe.
pub async fn public_key(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "key": st.push.public_key }))
}

#[derive(Deserialize)]
pub struct SubscribeBody {
    endpoint: String,
    keys: SubscribeKeys,
}

#[derive(Deserialize)]
pub struct SubscribeKeys {
    p256dh: String,
    auth: String,
}

/// Registers this browser for the signed-in user. Re-subscribing an
/// endpoint moves it to whoever is signed in now.
pub async fn subscribe(State(st): State<AppState>, Me(me): Me, Json(body): Json<SubscribeBody>) -> AppResult<Json<Value>> {
    let endpoint: http::Uri = body.endpoint.parse().map_err(|_| AppError::bad("That isn't a push address."))?;
    let host = endpoint.host().unwrap_or("");
    // Push services are public HTTPS hosts; refuse anything that would make
    // the server call into the local network.
    if endpoint.scheme_str() != Some("https") || host.parse::<std::net::IpAddr>().is_ok() || host == "localhost" || !host.contains('.') {
        return Err(AppError::bad("That isn't a push address."));
    }
    let keys_ok = Base64UrlUnpadded::decode_vec(&body.keys.p256dh)
        .ok()
        .is_some_and(|k| p256::PublicKey::from_sec1_bytes(&k).is_ok())
        && Base64UrlUnpadded::decode_vec(&body.keys.auth).is_ok_and(|a| a.len() == 16);
    if !keys_ok {
        return Err(AppError::bad("Those push keys don't look right."));
    }
    sqlx::query(
        "INSERT INTO push_subscriptions (endpoint, user_id, p256dh, auth) VALUES ($1, $2, $3, $4)
         ON CONFLICT (endpoint) DO UPDATE SET user_id = $2, p256dh = $3, auth = $4",
    )
    .bind(&body.endpoint)
    .bind(me)
    .bind(&body.keys.p256dh)
    .bind(&body.keys.auth)
    .execute(&st.pool)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct UnsubscribeBody {
    endpoint: String,
}

/// Stops pushes to this browser (used on log out).
pub async fn unsubscribe(State(st): State<AppState>, Me(me): Me, Json(body): Json<UnsubscribeBody>) -> AppResult<Json<Value>> {
    sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = $1 AND user_id = $2")
        .bind(body.endpoint)
        .bind(me)
        .execute(&st.pool)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

/// Marks all of the signed-in user's notifications as read.
pub async fn mark_read(State(st): State<AppState>, Me(me): Me) -> AppResult<Json<Value>> {
    let mut c = st.pool.acquire().await?;
    sqlx::query("UPDATE notifications SET read_at = now() WHERE user_id = $1 AND read_at IS NULL")
        .bind(me)
        .execute(&mut *c)
        .await?;
    Ok(Json(snapshot::build(&mut c, &st, me).await?))
}
