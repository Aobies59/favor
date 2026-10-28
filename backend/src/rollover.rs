//! Day rollover ("midnight"): ends instances, charges misses, deals new tasks.

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde_json::json;
use sqlx::{PgConnection, PgPool};

use crate::{
    error::AppResult,
    model::{self, UNRESOLVED},
};

/// Advances the household by one day. Call inside a locked transaction.
pub async fn rollover(c: &mut PgConnection) -> AppResult<NaiveDate> {
    let day = model::today(c).await?;
    let next = day + Duration::days(1);
    let house = model::household(c).await?;
    let everyone = house.all_ids();
    let instances: HashMap<i32, model::Instance> =
        model::instances(c).await?.into_iter().map(|i| (i.task_id, i)).collect();

    for t in model::tasks(c).await? {
        let Some(it) = instances.get(&t.id) else {
            sqlx::query("UPDATE tasks SET due_in_days = GREATEST(0, due_in_days - 1) WHERE id = $1")
                .bind(t.id)
                .execute(&mut *c)
                .await?;
            continue;
        };

        if t.weekly() && it.days_left.unwrap_or(1) > 1 {
            sqlx::query("UPDATE instances SET days_left = days_left - 1 WHERE id = $1")
                .bind(it.id)
                .execute(&mut *c)
                .await?;
            continue;
        }

        if UNRESOLVED.contains(&it.status.as_str()) {
            let v = t.value();
            let mut lost = serde_json::Map::new();
            for other in house.others(it.assignee_id) {
                let held = model::balance(c, it.assignee_id, other).await?;
                lost.insert(other.to_string(), json!(held.min(v)));
                model::add_balance(c, it.assignee_id, other, -v).await?;
            }
            sqlx::query(
                "INSERT INTO missed (task_id, task_name, difficulty, assignee_id, date, lost_json, status)
                 VALUES ($1, $2, $3, $4, $5, $6, 'missed')",
            )
            .bind(t.id)
            .bind(&t.name)
            .bind(&t.difficulty)
            .bind(it.assignee_id)
            .bind(day)
            .bind(serde_json::Value::Object(lost))
            .execute(&mut *c)
            .await?;
            let text = format!("{} missed {}", house.name(it.assignee_id), t.name);
            model::log(c, day, "miss", text, Some(format!("−{v} with everyone")), &everyone, Some(it.assignee_id))
                .await?;
        }

        sqlx::query("DELETE FROM instances WHERE id = $1").bind(it.id).execute(&mut *c).await?;
        let due = if t.weekly() { 0 } else { t.period() - 1 };
        let assignee = if t.rotates() { house.next_user(t.assignee_id) } else { t.assignee_id };
        sqlx::query("UPDATE tasks SET due_in_days = $2, assignee_id = $3 WHERE id = $1")
            .bind(t.id)
            .bind(due)
            .bind(assignee)
            .execute(&mut *c)
            .await?;
    }

    // Deal the new day.
    let carried: Vec<(i32,)> = sqlx::query_as("SELECT task_id FROM instances").fetch_all(&mut *c).await?;
    let carried: Vec<i32> = carried.into_iter().map(|(id,)| id).collect();
    for mut t in model::tasks(c).await? {
        if t.rotates() && house.away.contains(&t.assignee_id) {
            t.assignee_id = house.next_user(t.assignee_id);
            sqlx::query("UPDATE tasks SET assignee_id = $2 WHERE id = $1")
                .bind(t.id)
                .bind(t.assignee_id)
                .execute(&mut *c)
                .await?;
        }
        if t.due_in_days == 0 && !house.away.contains(&t.assignee_id) && !carried.contains(&t.id) {
            model::create_instance(c, &t, t.assignee_id, next).await?;
        }
    }

    sqlx::query("UPDATE app_state SET today = $1 WHERE id = 1").bind(next).execute(&mut *c).await?;
    Ok(next)
}

pub fn local_today(tz: Tz) -> NaiveDate {
    Utc::now().with_timezone(&tz).date_naive()
}

/// Makes sure the logical day exists, then catches up any midnights that
/// passed while the server was down.
pub async fn catch_up(pool: &PgPool, tz: Tz) -> AppResult<()> {
    let local = local_today(tz);
    let mut tx = pool.begin().await?;
    model::lock(&mut tx).await?;
    sqlx::query("INSERT INTO app_state (id, today) VALUES (1, $1) ON CONFLICT (id) DO NOTHING")
        .bind(local)
        .execute(&mut *tx)
        .await?;
    let mut day = model::today(&mut tx).await?;
    while day < local {
        day = rollover(&mut tx).await?;
        tracing::info!("rolled over to {day}");
    }
    tx.commit().await?;
    Ok(())
}

/// Checks every 30 seconds whether local midnight has passed.
pub fn spawn_scheduler(pool: PgPool, tz: Tz) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            if let Err(e) = catch_up(&pool, tz).await {
                tracing::error!("scheduled rollover failed: {}", e.message);
            }
        }
    });
}
