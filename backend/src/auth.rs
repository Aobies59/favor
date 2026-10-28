//! Accounts, password login and cookie sessions.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    async_trait,
    extract::{FromRequestParts, State},
    http::{header, request::Parts, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

use crate::{error::AppError, model::User, AppState};

const COOKIE: &str = "favor_session";

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

/// First-run seed: if there are no accounts yet, creates the ones listed in
/// `FAVOR_USERS` (`Name:password,Name:password,...`, in rotation order).
/// After that, accounts are managed on the admin page and this is ignored.
pub async fn seed_users(pool: &PgPool, spec: &str) -> anyhow::Result<()> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users").fetch_one(pool).await?;
    if count > 0 {
        return Ok(());
    }
    for (position, entry) in spec.split(',').map(str::trim).filter(|e| !e.is_empty()).enumerate() {
        let (name, password) = entry
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("FAVOR_USERS entry {entry:?} must look like Name:password"))?;
        let (name, password) = (name.trim(), password.trim());
        anyhow::ensure!(!name.is_empty() && !password.is_empty(), "FAVOR_USERS entry {entry:?} is incomplete");
        sqlx::query("INSERT INTO users (name, password_hash, position) VALUES ($1, $2, $3)")
            .bind(name)
            .bind(hash_password(password)?)
            .bind(position as i32)
            .execute(pool)
            .await?;
        tracing::info!("created account for {name}");
    }
    let mut c = pool.acquire().await?;
    ensure_balances(&mut c).await?;
    Ok(())
}

/// Every pair gets a balance row so the ledger always has both directions.
pub async fn ensure_balances(c: &mut sqlx::PgConnection) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO balances (holder_id, debtor_id, amount)
         SELECT a.id, b.id, 0 FROM users a CROSS JOIN users b WHERE a.id <> b.id
         ON CONFLICT DO NOTHING",
    )
    .execute(c)
    .await?;
    Ok(())
}

/// The signed-in user's id, taken from the session cookie.
pub struct Me(pub i32);

#[async_trait]
impl FromRequestParts<AppState> for Me {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = session_token(&parts.headers).ok_or_else(AppError::unauthorized)?;
        let row: Option<(i32,)> = sqlx::query_as("SELECT user_id FROM sessions WHERE token = $1")
            .bind(token)
            .fetch_optional(&state.pool)
            .await?;
        row.map(|(id,)| Me(id)).ok_or_else(AppError::unauthorized)
    }
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, COOKIE)
}

pub fn set_cookie(state: &AppState, name: &str, token: &str, max_age: i64) -> HeaderValue {
    let secure = if state.config.cookie_secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
    ))
    .expect("cookie is ASCII")
}

pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Public list of accounts for the login picker.
pub async fn list_users(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    let users = sqlx::query_as::<_, User>("SELECT id, name FROM users ORDER BY position, id")
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(json!({
        "house": state.config.house_name,
        "users": users.iter().map(|u| json!({ "id": u.id, "name": u.name })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct LoginBody {
    user_id: i32,
    password: String,
}

pub async fn login(State(state): State<AppState>, Json(body): Json<LoginBody>) -> Result<Response, AppError> {
    let row: Option<(String,)> = sqlx::query_as("SELECT password_hash FROM users WHERE id = $1")
        .bind(body.user_id)
        .fetch_optional(&state.pool)
        .await?;
    let ok = row.is_some_and(|(hash,)| verify_password(&body.password, &hash));
    if !ok {
        return Err(AppError::forbidden("Wrong password. Nice try."));
    }
    let token = new_token();
    sqlx::query("INSERT INTO sessions (token, user_id) VALUES ($1, $2)")
        .bind(&token)
        .bind(body.user_id)
        .execute(&state.pool)
        .await?;
    let mut res = Json(json!({ "ok": true })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, set_cookie(&state, COOKIE, &token, 60 * 60 * 24 * 365));
    Ok(res)
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(token) = session_token(&headers) {
        sqlx::query("DELETE FROM sessions WHERE token = $1")
            .bind(token)
            .execute(&state.pool)
            .await?;
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, set_cookie(&state, COOKIE, "", 0));
    Ok(res)
}
