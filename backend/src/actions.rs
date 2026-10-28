//! Every write the app can make. Each handler runs in one locked
//! transaction, checks the house rules, and answers with a snapshot.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::{PgConnection, Postgres, Transaction};

use crate::{
    auth::Me,
    error::{AppError, AppResult},
    model::{self, Household, Instance, Task, TaskData, UNRESOLVED},
    snapshot, AppState,
};

type Tx = Transaction<'static, Postgres>;
type Reply = AppResult<Json<Value>>;

async fn begin(st: &AppState) -> AppResult<Tx> {
    let mut tx = st.pool.begin().await?;
    model::lock(&mut tx).await?;
    Ok(tx)
}

async fn finish(mut tx: Tx, st: &AppState, me: i32) -> Reply {
    let snap = snapshot::build(&mut tx, st, me).await?;
    tx.commit().await?;
    Ok(Json(snap))
}

pub async fn state(State(st): State<AppState>, Me(me): Me) -> Reply {
    let mut c = st.pool.acquire().await?;
    Ok(Json(snapshot::build(&mut c, &st, me).await?))
}

// ── Today: task instances ───────────────────────────────────────────────

/// Loads an instance and its task, plus the household and current day.
async fn load(c: &mut PgConnection, id: i32) -> AppResult<(Instance, Task, Household, chrono::NaiveDate)> {
    let it = model::instance(c, id).await?;
    let t = model::task(c, it.task_id).await?;
    let house = model::household(c).await?;
    let day = model::today(c).await?;
    Ok((it, t, house, day))
}

fn require_mine_open(it: &Instance, me: i32) -> AppResult<()> {
    if it.assignee_id != me {
        return Err(AppError::forbidden("That one isn't yours."));
    }
    if it.status != "open" {
        return Err(AppError::conflict("That task isn't open any more."));
    }
    Ok(())
}

fn require_target(house: &Household, me: i32, to: i32) -> AppResult<()> {
    if to == me || !house.exists(to) {
        return Err(AppError::bad("Pick another roommate."));
    }
    if house.away.contains(&to) {
        return Err(AppError::conflict(format!("{} is on holiday.", house.name(to))));
    }
    Ok(())
}

async fn set_instance(
    c: &mut PgConnection,
    id: i32,
    status: &str,
    assignee: i32,
    hand_to: Option<i32>,
    reason: Option<&str>,
) -> AppResult<()> {
    sqlx::query("UPDATE instances SET status = $2, assignee_id = $3, hand_to_id = $4, reason = $5 WHERE id = $1")
        .bind(id)
        .bind(status)
        .bind(assignee)
        .bind(hand_to)
        .bind(reason)
        .execute(c)
        .await?;
    Ok(())
}

pub async fn done(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    let mut tx = begin(&st).await?;
    let (it, t, house, day) = load(&mut tx, id).await?;
    require_mine_open(&it, me)?;
    set_instance(&mut tx, id, "done", me, None, None).await?;
    model::log(&mut tx, day, "done", format!("{} did {}", house.name(me), t.name), None, &[me], Some(me)).await?;
    finish(tx, &st, me).await
}

/// Undo a Done, or withdraw a pending hand-off / can't-do.
pub async fn undo(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    let mut tx = begin(&st).await?;
    let (it, _, _, _) = load(&mut tx, id).await?;
    if it.assignee_id != me {
        return Err(AppError::forbidden("That one isn't yours."));
    }
    if !["done", "handoff", "cantdo"].contains(&it.status.as_str()) {
        return Err(AppError::conflict("Nothing to undo."));
    }
    set_instance(&mut tx, id, "open", me, None, None).await?;
    finish(tx, &st, me).await
}

#[derive(Deserialize)]
pub struct ReasonBody {
    reason: String,
}

pub async fn cant_do(
    State(st): State<AppState>,
    Me(me): Me,
    Path(id): Path<i32>,
    Json(body): Json<ReasonBody>,
) -> Reply {
    let reason = body.reason.trim();
    if reason.is_empty() || reason.chars().count() > 300 {
        return Err(AppError::bad("Give a reason (300 characters max)."));
    }
    let mut tx = begin(&st).await?;
    let (it, t, house, day) = load(&mut tx, id).await?;
    require_mine_open(&it, me)?;
    set_instance(&mut tx, id, "cantdo", me, None, Some(reason)).await?;
    let text = format!("{} can't do {}: \"{}\"", house.name(me), t.name, reason);
    model::log(&mut tx, day, "cant", text, None, &[me], Some(me)).await?;
    finish(tx, &st, me).await
}

/// Excuse (`yes`) or deny someone else's can't-do request.
async fn decide_cant(st: AppState, me: i32, id: i32, yes: bool) -> Reply {
    let mut tx = begin(&st).await?;
    let (it, t, house, day) = load(&mut tx, id).await?;
    if it.status != "cantdo" {
        return Err(AppError::conflict("Someone already dealt with that."));
    }
    if it.assignee_id == me {
        return Err(AppError::forbidden("You can't excuse yourself. Nice try."));
    }
    let who = it.assignee_id;
    let text = if yes {
        set_instance(&mut tx, id, "excused", who, None, it.reason.as_deref()).await?;
        format!("{} excused {} from {}", house.name(me), house.name(who), t.name)
    } else {
        set_instance(&mut tx, id, "open", who, None, None).await?;
        format!("{} didn't excuse {} from {}", house.name(me), house.name(who), t.name)
    };
    model::log(&mut tx, day, "cant", text, None, &[me, who], Some(me)).await?;
    finish(tx, &st, me).await
}

pub async fn excuse(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    decide_cant(st, me, id, true).await
}

pub async fn deny(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    decide_cant(st, me, id, false).await
}

#[derive(Deserialize)]
pub struct ToBody {
    to: i32,
}

pub async fn ask(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>, Json(body): Json<ToBody>) -> Reply {
    let mut tx = begin(&st).await?;
    let (it, t, house, day) = load(&mut tx, id).await?;
    require_mine_open(&it, me)?;
    require_target(&house, me, body.to)?;
    set_instance(&mut tx, id, "handoff", me, Some(body.to), None).await?;
    let text = format!("{} asked {} to take {}", house.name(me), house.name(body.to), t.name);
    model::log(&mut tx, day, "ask", text, None, &[me, body.to], Some(me)).await?;
    finish(tx, &st, me).await
}

pub async fn spend(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>, Json(body): Json<ToBody>) -> Reply {
    let mut tx = begin(&st).await?;
    let (it, t, house, day) = load(&mut tx, id).await?;
    require_mine_open(&it, me)?;
    require_target(&house, me, body.to)?;
    let v = t.value();
    let held = model::balance(&mut tx, me, body.to).await?;
    if held < v {
        return Err(AppError::conflict(format!("You need {v}, you hold {held}.")));
    }
    model::add_balance(&mut tx, me, body.to, -v).await?;
    set_instance(&mut tx, id, "open", body.to, None, None).await?;
    let (from, to) = (house.name(me), house.name(body.to));
    let text = format!("{from} spent favor to give {} to {to}", t.name);
    model::log(&mut tx, day, "spend", text, Some(format!("{from} −{v} on {to}")), &[me, body.to], Some(me)).await?;
    finish(tx, &st, me).await
}

async fn decide_handoff(st: AppState, me: i32, id: i32, take: bool) -> Reply {
    let mut tx = begin(&st).await?;
    let (it, t, house, day) = load(&mut tx, id).await?;
    if it.status != "handoff" || it.hand_to_id != Some(me) {
        return Err(AppError::conflict("That request isn't waiting on you any more."));
    }
    let from = it.assignee_id;
    if take {
        let v = t.value();
        model::add_balance(&mut tx, me, from, v).await?;
        set_instance(&mut tx, id, "open", me, None, None).await?;
        let (a, b) = (house.name(me), house.name(from));
        let text = format!("{a} took {} from {b}", t.name);
        model::log(&mut tx, day, "favor", text, Some(format!("{a} +{v} on {b}")), &[me, from], Some(me)).await?;
    } else {
        set_instance(&mut tx, id, "open", from, None, None).await?;
        let text = format!("{} declined {}", house.name(me), t.name);
        model::log(&mut tx, day, "ask", text, None, &[me, from], Some(me)).await?;
    }
    finish(tx, &st, me).await
}

pub async fn accept(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    decide_handoff(st, me, id, true).await
}

pub async fn decline(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    decide_handoff(st, me, id, false).await
}

// ── Missed ──────────────────────────────────────────────────────────────

async fn missed(c: &mut PgConnection, id: i32) -> AppResult<(i32, String, String, serde_json::Value)> {
    sqlx::query_as("SELECT assignee_id, task_name, status, lost_json FROM missed WHERE id = $1")
        .bind(id)
        .fetch_optional(c)
        .await?
        .ok_or_else(AppError::not_found)
}

pub async fn claim_missed(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    let mut tx = begin(&st).await?;
    let (who, name, status, _) = missed(&mut tx, id).await?;
    if who != me || status != "missed" {
        return Err(AppError::conflict("You can't claim that one."));
    }
    sqlx::query("UPDATE missed SET status = 'claimed' WHERE id = $1").bind(id).execute(&mut *tx).await?;
    let house = model::household(&mut tx).await?;
    let day = model::today(&mut tx).await?;
    let text = format!("{} says they did {} but forgot to tick it", house.name(me), name);
    model::log(&mut tx, day, "cant", text, None, &[me], Some(me)).await?;
    finish(tx, &st, me).await
}

pub async fn dismiss_missed(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    let mut tx = begin(&st).await?;
    let (who, _, status, _) = missed(&mut tx, id).await?;
    if who != me || status == "claimed" {
        return Err(AppError::conflict("You can't dismiss that one."));
    }
    sqlx::query("DELETE FROM missed WHERE id = $1").bind(id).execute(&mut *tx).await?;
    finish(tx, &st, me).await
}

async fn decide_missed(st: AppState, me: i32, id: i32, yes: bool) -> Reply {
    let mut tx = begin(&st).await?;
    let (who, name, status, lost) = missed(&mut tx, id).await?;
    if status != "claimed" {
        return Err(AppError::conflict("Someone already dealt with that."));
    }
    if who == me {
        return Err(AppError::forbidden("You can't vouch for yourself."));
    }
    let house = model::household(&mut tx).await?;
    let day = model::today(&mut tx).await?;
    if yes {
        let mut total = 0;
        for (other, amount) in lost.as_object().into_iter().flatten() {
            let (Ok(other), Some(amount)) = (other.parse::<i32>(), amount.as_i64()) else { continue };
            if amount > 0 && house.exists(other) {
                model::add_balance(&mut tx, who, other, amount as i32).await?;
                total += amount;
            }
        }
        sqlx::query("DELETE FROM missed WHERE id = $1").bind(id).execute(&mut *tx).await?;
        let text = format!("{} confirmed {} did {}", house.name(me), house.name(who), name);
        let delta = (total > 0).then(|| format!("{} gets {total} back", house.name(who)));
        model::log(&mut tx, day, "favor", text, delta, &[me, who], Some(who)).await?;
    } else {
        sqlx::query("UPDATE missed SET status = 'denied' WHERE id = $1").bind(id).execute(&mut *tx).await?;
        let text = format!("{} didn't confirm {} did {}", house.name(me), house.name(who), name);
        model::log(&mut tx, day, "cant", text, None, &[me, who], Some(me)).await?;
    }
    finish(tx, &st, me).await
}

pub async fn confirm_missed(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    decide_missed(st, me, id, true).await
}

pub async fn reject_missed(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>) -> Reply {
    decide_missed(st, me, id, false).await
}

// ── Holidays ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct NoteBody {
    #[serde(default)]
    note: String,
}

async fn holiday_status(c: &mut PgConnection, user: i32) -> AppResult<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT status FROM holidays WHERE user_id = $1")
        .bind(user)
        .fetch_optional(c)
        .await?;
    Ok(row.map(|(s,)| s))
}

pub async fn request_holiday(State(st): State<AppState>, Me(me): Me, Json(body): Json<NoteBody>) -> Reply {
    let note = body.note.trim();
    if note.chars().count() > 300 {
        return Err(AppError::bad("Keep the note under 300 characters."));
    }
    let mut tx = begin(&st).await?;
    if holiday_status(&mut tx, me).await?.is_some() {
        return Err(AppError::conflict("You already have a holiday on the go."));
    }
    sqlx::query("INSERT INTO holidays (user_id, status, note) VALUES ($1, 'requested', $2)")
        .bind(me)
        .bind(note)
        .execute(&mut *tx)
        .await?;
    let house = model::household(&mut tx).await?;
    let day = model::today(&mut tx).await?;
    let suffix = if note.is_empty() { String::new() } else { format!(": \"{note}\"") };
    let text = format!("{} asked to go on holiday{suffix}", house.name(me));
    model::log(&mut tx, day, "cant", text, None, &[me], Some(me)).await?;
    finish(tx, &st, me).await
}

pub async fn withdraw_holiday(State(st): State<AppState>, Me(me): Me) -> Reply {
    let mut tx = begin(&st).await?;
    sqlx::query("DELETE FROM holidays WHERE user_id = $1 AND status = 'requested'")
        .bind(me)
        .execute(&mut *tx)
        .await?;
    finish(tx, &st, me).await
}

async fn decide_holiday(st: AppState, me: i32, user: i32, yes: bool) -> Reply {
    let mut tx = begin(&st).await?;
    if user == me {
        return Err(AppError::forbidden("Someone else has to approve your holiday."));
    }
    if holiday_status(&mut tx, user).await?.as_deref() != Some("requested") {
        return Err(AppError::conflict("That request isn't waiting any more."));
    }
    let day = model::today(&mut tx).await?;

    if !yes {
        sqlx::query("DELETE FROM holidays WHERE user_id = $1").bind(user).execute(&mut *tx).await?;
        let house = model::household(&mut tx).await?;
        let text = format!("{} turned down {}'s holiday", house.name(me), house.name(user));
        model::log(&mut tx, day, "cant", text, None, &[me, user], Some(me)).await?;
        return finish(tx, &st, me).await;
    }

    sqlx::query("UPDATE holidays SET status = 'away', since = $2 WHERE user_id = $1")
        .bind(user)
        .bind(day)
        .execute(&mut *tx)
        .await?;
    // Reload so `away` includes them and the rotation skips them.
    let house = model::household(&mut tx).await?;
    let next = house.next_user(user);

    for it in model::instances(&mut tx).await? {
        if it.status == "handoff" && it.hand_to_id == Some(user) {
            // Nobody can answer a request sent to someone who's away.
            set_instance(&mut tx, it.id, "open", it.assignee_id, None, None).await?;
            continue;
        }
        if it.assignee_id != user || !UNRESOLVED.contains(&it.status.as_str()) {
            continue;
        }
        let t = model::task(&mut tx, it.task_id).await?;
        if t.rotates() && next != user {
            set_instance(&mut tx, it.id, "open", next, None, None).await?;
        } else {
            set_instance(&mut tx, it.id, "excused", user, None, it.reason.as_deref()).await?;
        }
    }
    sqlx::query("UPDATE tasks SET assignee_id = $2 WHERE mode = 'rotate' AND assignee_id = $1")
        .bind(user)
        .bind(next)
        .execute(&mut *tx)
        .await?;

    let text = format!("{} approved {}'s holiday. Rotating tasks moved on.", house.name(me), house.name(user));
    model::log(&mut tx, day, "vote", text, None, &house.all_ids(), Some(me)).await?;
    finish(tx, &st, me).await
}

pub async fn approve_holiday(State(st): State<AppState>, Me(me): Me, Path(user): Path<i32>) -> Reply {
    decide_holiday(st, me, user, true).await
}

pub async fn decline_holiday(State(st): State<AppState>, Me(me): Me, Path(user): Path<i32>) -> Reply {
    decide_holiday(st, me, user, false).await
}

pub async fn come_back(State(st): State<AppState>, Me(me): Me) -> Reply {
    let mut tx = begin(&st).await?;
    if holiday_status(&mut tx, me).await?.as_deref() != Some("away") {
        return Err(AppError::conflict("You're not on holiday."));
    }
    sqlx::query("DELETE FROM holidays WHERE user_id = $1").bind(me).execute(&mut *tx).await?;
    let day = model::today(&mut tx).await?;
    for t in model::tasks(&mut tx).await? {
        if !t.rotates() && t.assignee_id == me && t.due_in_days == 0 {
            model::create_instance(&mut tx, &t, me, day).await?;
        }
    }
    let house = model::household(&mut tx).await?;
    let text = format!("{} is back from holiday", house.name(me));
    model::log(&mut tx, day, "done", text, None, &house.all_ids(), Some(me)).await?;
    finish(tx, &st, me).await
}

// ── Editor: proposals and votes ─────────────────────────────────────────

#[derive(Deserialize)]
pub struct ProposeBody {
    kind: String,
    #[serde(rename = "taskId")]
    task_id: Option<i32>,
    data: Option<TaskData>,
}

async fn has_pending(c: &mut PgConnection, task_id: i32) -> AppResult<bool> {
    let row: Option<(i32,)> = sqlx::query_as("SELECT id FROM proposals WHERE task_id = $1 LIMIT 1")
        .bind(task_id)
        .fetch_optional(c)
        .await?;
    Ok(row.is_some())
}

pub async fn propose(State(st): State<AppState>, Me(me): Me, Json(body): Json<ProposeBody>) -> Reply {
    let mut tx = begin(&st).await?;
    let house = model::household(&mut tx).await?;
    let day = model::today(&mut tx).await?;

    let (task_id, data, title) = match body.kind.as_str() {
        "add" => {
            let d = body.data.ok_or_else(|| AppError::bad("Missing task details."))?.validated(&house)?;
            let title = d.name.clone();
            (None, Some(d), title)
        }
        "edit" | "delete" => {
            let id = body.task_id.ok_or_else(|| AppError::bad("Which task?"))?;
            let t = model::task(&mut tx, id).await?;
            if has_pending(&mut tx, id).await? {
                return Err(AppError::conflict("That task already has a vote pending."));
            }
            let data = if body.kind == "edit" {
                let d = body.data.ok_or_else(|| AppError::bad("Missing task details."))?.validated(&house)?;
                if d == TaskData::from_task(&t) {
                    return Err(AppError::bad("Nothing changed."));
                }
                Some(d)
            } else {
                None
            };
            (Some(id), data, t.name)
        }
        _ => return Err(AppError::bad("Unknown proposal kind.")),
    };

    let (pid,): (i32,) = sqlx::query_as(
        "INSERT INTO proposals (kind, task_id, data_json, proposer_id) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(&body.kind)
    .bind(task_id)
    .bind(data.map(|d| serde_json::to_value(d).expect("TaskData serialises")))
    .bind(me)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO votes (proposal_id, user_id, yes) VALUES ($1, $2, true)")
        .bind(pid)
        .bind(me)
        .execute(&mut *tx)
        .await?;
    let what = match body.kind.as_str() {
        "add" => "a new task",
        "edit" => "changes to",
        _ => "removing",
    };
    let text = format!("{} proposed {what}: {title}", house.name(me));
    model::log(&mut tx, day, "vote", text, None, &house.all_ids(), Some(me)).await?;
    apply_if_unanimous(&mut tx, pid, &house, day).await?;
    finish(tx, &st, me).await
}

#[derive(Deserialize)]
pub struct VoteBody {
    yes: bool,
}

#[derive(sqlx::FromRow)]
struct Proposal {
    kind: String,
    task_id: Option<i32>,
    data_json: Option<Value>,
}

async fn proposal(c: &mut PgConnection, id: i32) -> AppResult<Proposal> {
    sqlx::query_as::<_, Proposal>("SELECT kind, task_id, data_json FROM proposals WHERE id = $1")
        .bind(id)
        .fetch_optional(c)
        .await?
        .ok_or_else(AppError::not_found)
}

async fn proposal_title(c: &mut PgConnection, p: &Proposal) -> AppResult<String> {
    if let Some(name) = p.data_json.as_ref().and_then(|d| d.get("name")).and_then(Value::as_str) {
        return Ok(name.to_string());
    }
    match p.task_id {
        Some(id) => Ok(model::task(c, id).await?.name),
        None => Ok("a task".into()),
    }
}

pub async fn vote(State(st): State<AppState>, Me(me): Me, Path(id): Path<i32>, Json(body): Json<VoteBody>) -> Reply {
    let mut tx = begin(&st).await?;
    let p = proposal(&mut tx, id).await?;
    let inserted = sqlx::query("INSERT INTO votes (proposal_id, user_id, yes) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(me)
        .bind(body.yes)
        .execute(&mut *tx)
        .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::conflict("You already voted on that."));
    }
    let house = model::household(&mut tx).await?;
    let day = model::today(&mut tx).await?;
    if body.yes {
        apply_if_unanimous(&mut tx, id, &house, day).await?;
    } else {
        let title = proposal_title(&mut tx, &p).await?;
        sqlx::query("DELETE FROM proposals WHERE id = $1").bind(id).execute(&mut *tx).await?;
        let text = format!("{} vetoed: {title}", house.name(me));
        model::log(&mut tx, day, "vote", text, None, &house.all_ids(), Some(me)).await?;
    }
    finish(tx, &st, me).await
}

/// Applies a proposal once every roommate has voted yes.
pub async fn apply_if_unanimous(c: &mut PgConnection, id: i32, house: &Household, day: chrono::NaiveDate) -> AppResult<()> {
    let (yes,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM votes WHERE proposal_id = $1 AND yes")
        .bind(id)
        .fetch_one(&mut *c)
        .await?;
    if (yes as usize) < house.users.len() {
        return Ok(());
    }
    let p = proposal(c, id).await?;
    let title = proposal_title(c, &p).await?;
    let data: Option<TaskData> = p.data_json.clone().and_then(|d| serde_json::from_value(d).ok());

    match (p.kind.as_str(), data, p.task_id) {
        ("add", Some(d), _) => create_task(c, &d, house, day).await?,
        ("edit", Some(d), Some(task_id)) => update_task(c, task_id, &d).await?,
        ("delete", _, Some(task_id)) => delete_task(c, task_id).await?,
        _ => return Err(anyhow::anyhow!("proposal {id} is malformed").into()),
    }
    sqlx::query("DELETE FROM proposals WHERE id = $1").bind(id).execute(&mut *c).await?;

    let what = match p.kind.as_str() {
        "add" => "new task",
        "edit" => "changes to",
        _ => "removing",
    };
    let text = format!("Everyone approved: {what} {title}");
    model::log(c, day, "vote", text, None, &house.all_ids(), None).await?;
    Ok(())
}


// ── Applying task changes (after a unanimous vote, or from the admin page) ──

/// Creates a task due today and deals its first card right away; it counts
/// like any other.
pub async fn create_task(c: &mut PgConnection, d: &TaskData, house: &Household, day: chrono::NaiveDate) -> AppResult<()> {
    let (task_id,): (i32,) = sqlx::query_as(
        "INSERT INTO tasks (name, difficulty, freq_type, freq_n, mode, assignee_id, due_in_days)
         VALUES ($1, $2, $3, $4, $5, $6, 0) RETURNING id",
    )
    .bind(&d.name)
    .bind(&d.diff)
    .bind(&d.freq.kind)
    .bind(d.freq_n())
    .bind(&d.mode)
    .bind(d.owner)
    .fetch_one(&mut *c)
    .await?;
    let mut t = model::task(c, task_id).await?;
    if t.rotates() && house.away.contains(&t.assignee_id) {
        t.assignee_id = house.next_user(t.assignee_id);
        sqlx::query("UPDATE tasks SET assignee_id = $2 WHERE id = $1")
            .bind(t.id)
            .bind(t.assignee_id)
            .execute(&mut *c)
            .await?;
    }
    // A fixed task whose owner is away waits; "I'm back" deals it.
    if !house.away.contains(&t.assignee_id) {
        model::create_instance(c, &t, t.assignee_id, day).await?;
    }
    Ok(())
}

pub async fn update_task(c: &mut PgConnection, task_id: i32, d: &TaskData) -> AppResult<()> {
    sqlx::query(
        "UPDATE tasks SET name = $2, difficulty = $3, freq_type = $4, freq_n = $5, mode = $6, assignee_id = $7
         WHERE id = $1",
    )
    .bind(task_id)
    .bind(&d.name)
    .bind(&d.diff)
    .bind(&d.freq.kind)
    .bind(d.freq_n())
    .bind(&d.mode)
    .bind(d.owner)
    .execute(c)
    .await?;
    Ok(())
}

/// Removes a task; cascades to today's card and any proposals about it.
pub async fn delete_task(c: &mut PgConnection, task_id: i32) -> AppResult<()> {
    sqlx::query("DELETE FROM tasks WHERE id = $1").bind(task_id).execute(c).await?;
    Ok(())
}
