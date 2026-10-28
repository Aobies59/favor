//! Admin page API: manage roommate accounts. Signed in with
//! `FAVOR_ADMIN_PASSWORD`; disabled when that isn't set.

use axum::{
    async_trait,
    extract::{FromRequestParts, Path, State},
    http::{header, request::Parts, HeaderMap},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    actions,
    auth::{self, cookie_value, hash_password, new_token, set_cookie},
    error::{AppError, AppResult},
    model, AppState,
};

const COOKIE: &str = "favor_admin";
const SESSION_HOURS: i64 = 12;

fn enabled(st: &AppState) -> AppResult<&str> {
    st.config
        .admin_password
        .as_deref()
        .ok_or_else(|| AppError::forbidden("The admin page is off. Set FAVOR_ADMIN_PASSWORD to turn it on."))
}

/// Compares without bailing out at the first differing byte.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Proof that the request carries a live admin session.
pub struct Admin;

#[async_trait]
impl FromRequestParts<AppState> for Admin {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        enabled(st)?;
        let token = cookie_value(&parts.headers, COOKIE).ok_or_else(AppError::unauthorized)?;
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM admin_sessions WHERE token = $1 AND created_at > now() - make_interval(hours => $2)",
        )
        .bind(token)
        .bind(SESSION_HOURS as i32)
        .fetch_optional(&st.pool)
        .await?;
        row.map(|_| Admin).ok_or_else(AppError::unauthorized)
    }
}

#[derive(Deserialize)]
pub struct LoginBody {
    password: String,
}

pub async fn login(State(st): State<AppState>, Json(body): Json<LoginBody>) -> Result<Response, AppError> {
    let expected = enabled(&st)?;
    if !same(&body.password, expected) {
        // Slow down guessing.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return Err(AppError::forbidden("Wrong admin password."));
    }
    sqlx::query("DELETE FROM admin_sessions WHERE created_at < now() - make_interval(hours => $1)")
        .bind(SESSION_HOURS as i32)
        .execute(&st.pool)
        .await?;
    let token = new_token();
    sqlx::query("INSERT INTO admin_sessions (token) VALUES ($1)").bind(&token).execute(&st.pool).await?;
    let mut res = Json(json!({ "ok": true })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, set_cookie(&st, COOKIE, &token, SESSION_HOURS * 3600));
    Ok(res)
}

pub async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(token) = cookie_value(&headers, COOKIE) {
        sqlx::query("DELETE FROM admin_sessions WHERE token = $1").bind(token).execute(&st.pool).await?;
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    res.headers_mut().insert(header::SET_COOKIE, set_cookie(&st, COOKIE, "", 0));
    Ok(res)
}

pub async fn list(State(st): State<AppState>, _: Admin) -> AppResult<Json<Value>> {
    let mut c = st.pool.acquire().await?;
    users_json(&mut c).await.map(Json)
}

async fn users_json(c: &mut sqlx::PgConnection) -> AppResult<Value> {
    let rows: Vec<(i32, String, i64)> = sqlx::query_as(
        "SELECT u.id, u.name, (SELECT COUNT(*) FROM sessions s WHERE s.user_id = u.id)
         FROM users u ORDER BY u.position, u.id",
    )
    .fetch_all(c)
    .await?;
    Ok(json!({
        "users": rows.iter().map(|(id, name, sessions)| json!({ "id": id, "name": name, "sessions": sessions })).collect::<Vec<_>>(),
    }))
}

fn clean_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(AppError::bad("Names need 1 to 40 characters."));
    }
    Ok(name.to_string())
}

fn check_password(pw: &str) -> AppResult<()> {
    if pw.chars().count() < 4 {
        return Err(AppError::bad("Passwords need at least 4 characters."));
    }
    Ok(())
}

fn name_taken(e: sqlx::Error) -> AppError {
    match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => AppError::conflict("Someone already has that name."),
        _ => e.into(),
    }
}

#[derive(Deserialize)]
pub struct CreateBody {
    name: String,
    password: String,
}

pub async fn create(State(st): State<AppState>, _: Admin, Json(body): Json<CreateBody>) -> AppResult<Json<Value>> {
    let name = clean_name(&body.name)?;
    check_password(&body.password)?;
    let mut tx = st.pool.begin().await?;
    model::lock(&mut tx).await?;
    sqlx::query(
        "INSERT INTO users (name, password_hash, position)
         VALUES ($1, $2, (SELECT COALESCE(MAX(position) + 1, 0) FROM users))",
    )
    .bind(&name)
    .bind(hash_password(&body.password)?)
    .execute(&mut *tx)
    .await
    .map_err(name_taken)?;
    auth::ensure_balances(&mut tx).await?;
    let house = model::household(&mut tx).await?;
    let day = model::today(&mut tx).await?;
    model::log(&mut tx, day, "done", format!("{name} moved in"), None, &house.all_ids(), None).await?;
    let out = users_json(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct UpdateBody {
    name: Option<String>,
    password: Option<String>,
}

/// Rename and/or set a new password. A new password signs them out everywhere.
pub async fn update(
    State(st): State<AppState>,
    _: Admin,
    Path(id): Path<i32>,
    Json(body): Json<UpdateBody>,
) -> AppResult<Json<Value>> {
    let mut tx = st.pool.begin().await?;
    model::lock(&mut tx).await?;
    if !model::household(&mut tx).await?.exists(id) {
        return Err(AppError::not_found());
    }
    if let Some(name) = body.name.as_deref() {
        let name = clean_name(name)?;
        sqlx::query("UPDATE users SET name = $2 WHERE id = $1")
            .bind(id)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(name_taken)?;
    }
    if let Some(pw) = body.password.as_deref().filter(|p| !p.is_empty()) {
        check_password(pw)?;
        sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
            .bind(id)
            .bind(hash_password(pw)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id = $1").bind(id).execute(&mut *tx).await?;
    }
    let out = users_json(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct MoveBody {
    /// -1 moves earlier in the rotation, 1 later.
    dir: i32,
}

pub async fn reorder(
    State(st): State<AppState>,
    _: Admin,
    Path(id): Path<i32>,
    Json(body): Json<MoveBody>,
) -> AppResult<Json<Value>> {
    let mut tx = st.pool.begin().await?;
    model::lock(&mut tx).await?;
    let mut ids = model::household(&mut tx).await?.all_ids();
    let i = ids.iter().position(|&u| u == id).ok_or_else(AppError::not_found)?;
    let j = i as i64 + body.dir.signum() as i64;
    if (0..ids.len() as i64).contains(&j) {
        ids.swap(i, j as usize);
    }
    for (pos, uid) in ids.iter().enumerate() {
        sqlx::query("UPDATE users SET position = $2 WHERE id = $1")
            .bind(uid)
            .bind(pos as i32)
            .execute(&mut *tx)
            .await?;
    }
    let out = users_json(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// Removes a roommate. Their tasks and today's instances pass to the next
/// person in the rotation; their missed records, proposals (and any that
/// would make them a task's owner), votes, holiday, balances and sessions
/// go with them.
pub async fn remove(State(st): State<AppState>, _: Admin, Path(id): Path<i32>) -> AppResult<Json<Value>> {
    let mut tx = st.pool.begin().await?;
    model::lock(&mut tx).await?;
    let mut house = model::household(&mut tx).await?;
    if !house.exists(id) {
        return Err(AppError::not_found());
    }
    if house.users.len() < 2 {
        return Err(AppError::conflict("Keep at least one roommate."));
    }
    let name = house.name(id).to_string();
    // Treat them as away so the rotation skips them; if everyone else is away
    // too, fall back to whoever is next in order.
    house.away.insert(id);
    let heir = match house.next_user(id) {
        n if n != id => n,
        _ => house.others(id).next().expect("at least two roommates"),
    };

    sqlx::query("UPDATE tasks SET assignee_id = $2 WHERE assignee_id = $1")
        .bind(id)
        .bind(heir)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE instances SET status = 'open', hand_to_id = NULL WHERE hand_to_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE instances SET assignee_id = $2, hand_to_id = NULL,
             status = CASE WHEN status IN ('handoff', 'cantdo') THEN 'open' ELSE status END,
             reason = CASE WHEN status IN ('handoff', 'cantdo') THEN NULL ELSE reason END
         WHERE assignee_id = $1",
    )
    .bind(id)
    .bind(heir)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM missed WHERE assignee_id = $1").bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM proposals WHERE proposer_id = $1 OR (data_json->>'owner')::int = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1").bind(id).execute(&mut *tx).await?;

    let house = model::household(&mut tx).await?;
    let day = model::today(&mut tx).await?;
    let text = format!("{name} moved out. Their tasks went to {}.", house.name(heir));
    model::log(&mut tx, day, "vote", text, None, &house.all_ids(), None).await?;

    // With one voter fewer, some pending proposals may now be unanimous.
    let pending: Vec<(i32,)> = sqlx::query_as("SELECT id FROM proposals ORDER BY id").fetch_all(&mut *tx).await?;
    for (pid,) in pending {
        actions::apply_if_unanimous(&mut tx, pid, &house, day).await?;
    }

    let out = users_json(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(out))
}
