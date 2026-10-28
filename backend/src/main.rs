mod actions;
mod admin;
mod auth;
mod error;
mod model;
mod rollover;
mod snapshot;

use std::{env, sync::Arc, time::Duration};

use anyhow::Context;
use axum::{
    response::Redirect,
    routing::{get, post},
    Router,
};
use chrono_tz::Tz;
use sqlx::{postgres::PgPoolOptions, PgPool};
use tower_http::{services::ServeDir, trace::TraceLayer};

pub struct Config {
    pub house_name: String,
    pub cookie_secure: bool,
    pub admin_password: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

fn env_flag(key: &str) -> bool {
    matches!(env_or(key, "false").to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let mut attempt = 0;
    loop {
        match PgPoolOptions::new().max_connections(10).connect(url).await {
            Ok(pool) => return Ok(pool),
            Err(e) if attempt < 30 => {
                attempt += 1;
                tracing::warn!("database not ready ({e}), retrying…");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e).context("connecting to the database"),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(env_or("RUST_LOG", "favor=info,tower_http=warn"))
        .init();

    let database_url = env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let tz: Tz = env_or("FAVOR_TZ", "UTC")
        .parse()
        .map_err(|e| anyhow::anyhow!("FAVOR_TZ is not a valid timezone: {e}"))?;
    let config = Config {
        house_name: env_or("FAVOR_HOUSE_NAME", "Our flat"),
        cookie_secure: env_flag("FAVOR_COOKIE_SECURE"),
        admin_password: env::var("FAVOR_ADMIN_PASSWORD").ok().filter(|p| !p.trim().is_empty()),
    };

    let pool = connect(&database_url).await?;
    sqlx::migrate!().run(&pool).await.context("running migrations")?;
    auth::seed_users(&pool, &env_or("FAVOR_USERS", "")).await?;
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users").fetch_one(&pool).await?;
    if count == 0 {
        tracing::warn!("no accounts yet: add roommates on /admin (or set FAVOR_USERS before first start)");
    }
    if config.admin_password.is_none() {
        tracing::warn!("FAVOR_ADMIN_PASSWORD is not set, so the admin page is disabled");
    }
    rollover::catch_up(&pool, tz).await.map_err(|e| anyhow::anyhow!(e.message))?;
    rollover::spawn_scheduler(pool.clone(), tz);

    let state = AppState { pool, config: Arc::new(config) };
    let static_dir = env_or("FAVOR_STATIC_DIR", "static");

    let api = Router::new()
        .route("/users", get(auth::list_users))
        .route("/login", post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/state", get(actions::state))
        .route("/items/:id/done", post(actions::done))
        .route("/items/:id/undo", post(actions::undo))
        .route("/items/:id/cantdo", post(actions::cant_do))
        .route("/items/:id/excuse", post(actions::excuse))
        .route("/items/:id/deny", post(actions::deny))
        .route("/items/:id/ask", post(actions::ask))
        .route("/items/:id/accept", post(actions::accept))
        .route("/items/:id/decline", post(actions::decline))
        .route("/items/:id/spend", post(actions::spend))
        .route("/missed/:id/claim", post(actions::claim_missed))
        .route("/missed/:id/dismiss", post(actions::dismiss_missed))
        .route("/missed/:id/confirm", post(actions::confirm_missed))
        .route("/missed/:id/reject", post(actions::reject_missed))
        .route("/holiday", post(actions::request_holiday))
        .route("/holiday/withdraw", post(actions::withdraw_holiday))
        .route("/holiday/back", post(actions::come_back))
        .route("/holiday/:user/approve", post(actions::approve_holiday))
        .route("/holiday/:user/decline", post(actions::decline_holiday))
        .route("/proposals", post(actions::propose))
        .route("/proposals/:id/vote", post(actions::vote))
        .route("/admin/login", post(admin::login))
        .route("/admin/logout", post(admin::logout))
        .route("/admin/users", get(admin::list).post(admin::create))
        .route("/admin/users/:id", post(admin::update))
        .route("/admin/users/:id/move", post(admin::reorder))
        .route("/admin/users/:id/delete", post(admin::remove))
        .route("/admin/tasks", get(admin::tasks).post(admin::create_task))
        .route("/admin/tasks/:id", post(admin::update_task))
        .route("/admin/tasks/:id/delete", post(admin::delete_task));

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/admin", get(|| async { Redirect::permanent("/admin.html") }))
        .nest("/api", api)
        .fallback_service(ServeDir::new(&static_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", env_or("FAVOR_PORT", "8080"));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Favor listening on {addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Docker stops containers with SIGTERM; Ctrl-C covers local runs.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}
