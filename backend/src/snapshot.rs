//! The whole household as the frontend sees it. Every action responds with
//! a fresh snapshot so the page never has to stitch partial updates.

use std::collections::HashMap;

use chrono::NaiveDate;
use serde_json::{json, Value};
use sqlx::{FromRow, PgConnection};

use crate::{error::AppResult, model, AppState};

const LOG_LIMIT: i64 = 500;

#[derive(FromRow)]
struct MissedRow {
    id: i32,
    task_id: Option<i32>,
    task_name: String,
    difficulty: String,
    assignee_id: i32,
    date: NaiveDate,
    lost_json: Value,
    status: String,
}

#[derive(FromRow)]
struct ProposalRow {
    id: i32,
    kind: String,
    task_id: Option<i32>,
    data_json: Option<Value>,
    proposer_id: i32,
}

#[derive(FromRow)]
struct ActivityRow {
    date: NaiveDate,
    kind: String,
    text: String,
    delta: Option<String>,
    user_ids: Vec<i32>,
    actor_id: Option<i32>,
}

#[derive(FromRow)]
struct HolidayRow {
    user_id: i32,
    status: String,
    note: String,
    since: Option<NaiveDate>,
}

pub async fn build(c: &mut PgConnection, state: &AppState, me: i32) -> AppResult<Value> {
    let house = model::household(c).await?;
    let today = model::today(c).await?;

    let tasks: Vec<Value> = model::tasks(c)
        .await?
        .iter()
        .map(|t| {
            json!({
                "id": t.id, "name": t.name, "diff": t.difficulty,
                "freq": model::TaskData::from_task(t).freq,
                "mode": t.mode, "assignee": t.assignee_id, "due": t.due_in_days,
            })
        })
        .collect();

    let items: Vec<Value> = model::instances(c)
        .await?
        .iter()
        .map(|i| {
            json!({
                "id": i.id, "taskId": i.task_id, "assignee": i.assignee_id, "status": i.status,
                "handTo": i.hand_to_id, "reason": i.reason, "daysLeft": i.days_left,
            })
        })
        .collect();

    let missed: Vec<Value> = sqlx::query_as::<_, MissedRow>(
        "SELECT id, task_id, task_name, difficulty, assignee_id, date, lost_json, status
         FROM missed ORDER BY date DESC, id DESC",
    )
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .map(|m| {
        json!({
            "id": m.id, "taskId": m.task_id, "name": m.task_name, "diff": m.difficulty,
            "assignee": m.assignee_id, "day": m.date, "lost": m.lost_json, "status": m.status,
        })
    })
    .collect();

    let mut bal: HashMap<String, HashMap<String, i32>> = house
        .users
        .iter()
        .map(|u| (u.id.to_string(), house.others(u.id).map(|o| (o.to_string(), 0)).collect()))
        .collect();
    let rows: Vec<(i32, i32, i32)> = sqlx::query_as("SELECT holder_id, debtor_id, amount FROM balances")
        .fetch_all(&mut *c)
        .await?;
    for (holder, debtor, amount) in rows {
        bal.entry(holder.to_string()).or_default().insert(debtor.to_string(), amount);
    }

    let votes: Vec<(i32, i32, bool)> = sqlx::query_as("SELECT proposal_id, user_id, yes FROM votes")
        .fetch_all(&mut *c)
        .await?;
    let pending: Vec<Value> = sqlx::query_as::<_, ProposalRow>(
        "SELECT id, kind, task_id, data_json, proposer_id FROM proposals ORDER BY id",
    )
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .map(|p| {
        let v: serde_json::Map<String, Value> = house
            .users
            .iter()
            .map(|u| {
                let vote = votes.iter().find(|(pid, uid, _)| *pid == p.id && *uid == u.id);
                (u.id.to_string(), vote.map_or(Value::Null, |(_, _, yes)| json!(yes)))
            })
            .collect();
        json!({
            "id": p.id, "kind": p.kind, "taskId": p.task_id, "data": p.data_json,
            "by": p.proposer_id, "votes": v,
        })
    })
    .collect();

    let log: Vec<Value> = sqlx::query_as::<_, ActivityRow>(
        "SELECT date, kind, text, delta, user_ids, actor_id FROM activity ORDER BY id DESC LIMIT $1",
    )
    .bind(LOG_LIMIT)
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .map(|a| {
        json!({
            "day": a.date, "kind": a.kind, "text": a.text, "delta": a.delta,
            "who": a.user_ids, "actor": a.actor_id,
        })
    })
    .collect();

    let holiday: serde_json::Map<String, Value> =
        sqlx::query_as::<_, HolidayRow>("SELECT user_id, status, note, since FROM holidays")
            .fetch_all(&mut *c)
            .await?
            .into_iter()
            .map(|h| (h.user_id.to_string(), json!({ "status": h.status, "note": h.note, "since": h.since })))
            .collect();

    Ok(json!({
        "house": state.config.house_name,
        "me": me,
        "today": today,
        "users": house.users.iter().map(|u| json!({ "id": u.id, "name": u.name })).collect::<Vec<_>>(),
        "tasks": tasks,
        "items": items,
        "missed": missed,
        "bal": bal,
        "pending": pending,
        "log": log,
        "holiday": holiday,
    }))
}
