//! Domain types and the small database helpers every action shares.

use std::collections::HashSet;

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};

use crate::error::{AppError, AppResult};

/// Statuses that leave an instance unresolved at midnight.
pub const UNRESOLVED: [&str; 3] = ["open", "handoff", "cantdo"];

pub fn value_of(difficulty: &str) -> i32 {
    match difficulty {
        "easy" => 1,
        "medium" => 2,
        _ => 3,
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct Task {
    pub id: i32,
    pub name: String,
    pub difficulty: String,
    pub freq_type: String,
    pub freq_n: i32,
    pub mode: String,
    pub assignee_id: i32,
    pub due_in_days: i32,
    pub start_dow: Option<i32>,
}

impl Task {
    pub fn value(&self) -> i32 {
        value_of(&self.difficulty)
    }
    pub fn weekly(&self) -> bool {
        self.freq_type == "weekly"
    }
    pub fn rotates(&self) -> bool {
        self.mode == "rotate"
    }
    pub fn period(&self) -> i32 {
        match self.freq_type.as_str() {
            "daily" => 1,
            "weekly" => 7,
            _ => self.freq_n,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Instance {
    pub id: i32,
    pub task_id: i32,
    pub assignee_id: i32,
    pub status: String,
    pub hand_to_id: Option<i32>,
    pub reason: Option<String>,
    pub days_left: Option<i32>,
}

/// Task fields carried by add/edit proposals, in the shape the frontend uses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskData {
    pub name: String,
    pub diff: String,
    pub freq: Freq,
    pub mode: String,
    pub owner: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Freq {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<i32>,
    /// Weekly only: the weekday each window starts on (0 = Monday).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i32>,
}

impl TaskData {
    /// Validates and normalises client input.
    pub fn validated(mut self, house: &Household) -> AppResult<Self> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() || self.name.chars().count() > 80 {
            return Err(AppError::bad("Give the task a name (80 characters max)."));
        }
        if !["easy", "medium", "hard"].contains(&self.diff.as_str()) {
            return Err(AppError::bad("Unknown difficulty."));
        }
        if !["rotate", "fixed"].contains(&self.mode.as_str()) {
            return Err(AppError::bad("Unknown assignment."));
        }
        match self.freq.kind.as_str() {
            "daily" => {
                self.freq.n = None;
                self.freq.start = None;
            }
            "weekly" => {
                self.freq.n = None;
                if self.freq.start.is_some_and(|d| !(0..=6).contains(&d)) {
                    return Err(AppError::bad("Pick the day the week starts."));
                }
            }
            "every" => {
                self.freq.start = None;
                let n = self.freq.n.unwrap_or(2);
                if !(2..=365).contains(&n) {
                    return Err(AppError::bad("Every X days needs X between 2 and 365."));
                }
                self.freq.n = Some(n);
            }
            _ => return Err(AppError::bad("Unknown frequency.")),
        }
        if !house.users.iter().any(|u| u.id == self.owner) {
            return Err(AppError::bad("Pick a roommate."));
        }
        Ok(self)
    }

    pub fn from_task(t: &Task) -> Self {
        TaskData {
            name: t.name.clone(),
            diff: t.difficulty.clone(),
            freq: Freq {
                kind: t.freq_type.clone(),
                n: (t.freq_type == "every").then_some(t.freq_n),
                start: if t.weekly() { t.start_dow } else { None },
            },
            mode: t.mode.clone(),
            owner: t.assignee_id,
        }
    }

    pub fn freq_n(&self) -> i32 {
        self.freq.n.unwrap_or(1)
    }
}

/// Everyone in the flat, in rotation order, plus who's away.
pub struct Household {
    pub users: Vec<User>,
    pub away: HashSet<i32>,
}

impl Household {
    pub fn name(&self, id: i32) -> &str {
        self.users.iter().find(|u| u.id == id).map_or("Someone", |u| u.name.as_str())
    }

    pub fn exists(&self, id: i32) -> bool {
        self.users.iter().any(|u| u.id == id)
    }

    pub fn all_ids(&self) -> Vec<i32> {
        self.users.iter().map(|u| u.id).collect()
    }

    pub fn others(&self, id: i32) -> impl Iterator<Item = i32> + '_ {
        self.users.iter().map(|u| u.id).filter(move |&u| u != id)
    }

    /// The next roommate after `id` in rotation order who isn't away.
    /// Returns `id` itself when everyone else is away.
    pub fn next_user(&self, id: i32) -> i32 {
        let len = self.users.len();
        if len == 0 {
            return id;
        }
        let mut i = self.users.iter().position(|u| u.id == id).map_or(len - 1, |p| p);
        for _ in 0..len {
            i = (i + 1) % len;
            if !self.away.contains(&self.users[i].id) {
                return self.users[i].id;
            }
        }
        id
    }
}

/// Serialises every write. A household is a handful of people, so one
/// transaction-scoped lock keeps the favor rules simple and race-free.
pub async fn lock(c: &mut PgConnection) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(4242)").execute(c).await?;
    Ok(())
}

pub async fn household(c: &mut PgConnection) -> AppResult<Household> {
    let users = sqlx::query_as::<_, User>("SELECT id, name FROM users ORDER BY position, id")
        .fetch_all(&mut *c)
        .await?;
    let away: Vec<(i32,)> = sqlx::query_as("SELECT user_id FROM holidays WHERE status = 'away'")
        .fetch_all(&mut *c)
        .await?;
    Ok(Household { users, away: away.into_iter().map(|(id,)| id).collect() })
}

pub async fn today(c: &mut PgConnection) -> AppResult<NaiveDate> {
    let (d,): (NaiveDate,) = sqlx::query_as("SELECT today FROM app_state WHERE id = 1")
        .fetch_one(c)
        .await?;
    Ok(d)
}

pub async fn log(
    c: &mut PgConnection,
    date: NaiveDate,
    kind: &str,
    text: String,
    delta: Option<String>,
    users: &[i32],
    actor: Option<i32>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO activity (date, kind, text, delta, user_ids, actor_id) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(date)
    .bind(kind)
    .bind(text)
    .bind(delta)
    .bind(users)
    .bind(actor)
    .execute(c)
    .await?;
    Ok(())
}

pub async fn balance(c: &mut PgConnection, holder: i32, debtor: i32) -> AppResult<i32> {
    let row: Option<(i32,)> =
        sqlx::query_as("SELECT amount FROM balances WHERE holder_id = $1 AND debtor_id = $2")
            .bind(holder)
            .bind(debtor)
            .fetch_optional(c)
            .await?;
    Ok(row.map_or(0, |(a,)| a))
}

/// Adds `delta` (possibly negative) to what `holder` holds on `debtor`,
/// flooring at zero.
pub async fn add_balance(c: &mut PgConnection, holder: i32, debtor: i32, delta: i32) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO balances (holder_id, debtor_id, amount) VALUES ($1, $2, GREATEST(0, $3))
         ON CONFLICT (holder_id, debtor_id) DO UPDATE SET amount = GREATEST(0, balances.amount + $3)",
    )
    .bind(holder)
    .bind(debtor)
    .bind(delta)
    .execute(c)
    .await?;
    Ok(())
}

const TASK_COLS: &str = "id, name, difficulty, freq_type, freq_n, mode, assignee_id, due_in_days, start_dow";
const INSTANCE_COLS: &str = "id, task_id, assignee_id, status, hand_to_id, reason, days_left";

pub async fn tasks(c: &mut PgConnection) -> AppResult<Vec<Task>> {
    Ok(sqlx::query_as::<_, Task>(&format!("SELECT {TASK_COLS} FROM tasks ORDER BY id"))
        .fetch_all(c)
        .await?)
}

pub async fn task(c: &mut PgConnection, id: i32) -> AppResult<Task> {
    sqlx::query_as::<_, Task>(&format!("SELECT {TASK_COLS} FROM tasks WHERE id = $1"))
        .bind(id)
        .fetch_optional(c)
        .await?
        .ok_or_else(AppError::not_found)
}

pub async fn instances(c: &mut PgConnection) -> AppResult<Vec<Instance>> {
    Ok(sqlx::query_as::<_, Instance>(&format!("SELECT {INSTANCE_COLS} FROM instances ORDER BY id"))
        .fetch_all(c)
        .await?)
}

pub async fn instance(c: &mut PgConnection, id: i32) -> AppResult<Instance> {
    sqlx::query_as::<_, Instance>(&format!("SELECT {INSTANCE_COLS} FROM instances WHERE id = $1"))
        .bind(id)
        .fetch_optional(c)
        .await?
        .ok_or_else(AppError::not_found)
}

/// Creates today's instance of a task. Weekly tasks get a 7-day window.
pub async fn create_instance(c: &mut PgConnection, t: &Task, assignee: i32, date: NaiveDate) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO instances (task_id, date, assignee_id, status, days_left)
         VALUES ($1, $2, $3, 'open', $4) ON CONFLICT (task_id) DO NOTHING",
    )
    .bind(t.id)
    .bind(date)
    .bind(assignee)
    .bind(t.weekly().then(|| t.window_len(date)))
    .execute(c)
    .await?;
    Ok(())
}

/// Weekday index with Monday = 0, matching `tasks.start_dow`.
pub fn weekday(day: NaiveDate) -> i32 {
    day.weekday().num_days_from_monday() as i32
}

/// Days from `day` until the next `start` weekday (0 when `day` is one).
pub fn days_until(start: i32, day: NaiveDate) -> i32 {
    (start - weekday(day)).rem_euclid(7)
}

impl Task {
    /// Days a weekly window dealt on `day` lasts: up to the night before the
    /// next start day, so windows stay lined up with the chosen weekday.
    pub fn window_len(&self, day: NaiveDate) -> i32 {
        match self.start_dow.map(|s| days_until(s, day)) {
            Some(d) if d > 0 => d,
            _ => 7,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn house(away: &[i32]) -> Household {
        Household {
            users: [(1, "Marta"), (2, "Leo"), (3, "Inés")]
                .into_iter()
                .map(|(id, name)| User { id, name: name.into() })
                .collect(),
            away: away.iter().copied().collect(),
        }
    }

    fn data(freq: &str, n: Option<i32>) -> TaskData {
        TaskData {
            name: "  Dishes ".into(),
            diff: "medium".into(),
            freq: Freq { kind: freq.into(), n, start: None },
            mode: "rotate".into(),
            owner: 1,
        }
    }

    #[test]
    fn rotation_cycles_in_order() {
        let h = house(&[]);
        assert_eq!(h.next_user(1), 2);
        assert_eq!(h.next_user(3), 1);
    }

    #[test]
    fn rotation_skips_people_away() {
        let h = house(&[2]);
        assert_eq!(h.next_user(1), 3);
        assert_eq!(h.next_user(3), 1);
        // Everyone else away: it stays with the same person.
        assert_eq!(house(&[2, 3]).next_user(1), 1);
    }

    #[test]
    fn values_follow_difficulty() {
        assert_eq!((value_of("easy"), value_of("medium"), value_of("hard")), (1, 2, 3));
    }

    #[test]
    fn validation_normalises_and_rejects() {
        let h = house(&[]);
        let d = data("daily", Some(5)).validated(&h).unwrap();
        assert_eq!(d.name, "Dishes");
        assert_eq!(d.freq.n, None);
        assert_eq!(data("every", Some(4)).validated(&h).unwrap().freq_n(), 4);
        assert!(data("every", Some(1)).validated(&h).is_err());
        assert!(data("monthly", None).validated(&h).is_err());
        let mut bad_owner = data("weekly", None);
        bad_owner.owner = 9;
        assert!(bad_owner.validated(&h).is_err());
    }

    #[test]
    fn weekly_windows_line_up_with_the_start_day() {
        // 2026-10-29 is a Thursday.
        let thu = NaiveDate::from_ymd_opt(2026, 10, 29).unwrap();
        assert_eq!(weekday(thu), 3);
        assert_eq!(days_until(3, thu), 0);
        assert_eq!(days_until(4, thu), 1); // Friday is tomorrow
        assert_eq!(days_until(0, thu), 4); // next Monday

        let mut t = Task {
            id: 1,
            name: "Bathroom".into(),
            difficulty: "hard".into(),
            freq_type: "weekly".into(),
            freq_n: 1,
            mode: "fixed".into(),
            assignee_id: 1,
            due_in_days: 0,
            start_dow: Some(3),
        };
        assert_eq!(t.window_len(thu), 7); // dealt on its start day: a full week
        t.start_dow = Some(0);
        assert_eq!(t.window_len(thu), 4); // Thu–Sun, then Monday's window
        t.start_dow = None;
        assert_eq!(t.window_len(thu), 7);
    }

    #[test]
    fn start_day_only_kept_for_weekly() {
        let h = house(&[]);
        let mut d = data("weekly", None);
        d.freq.start = Some(4);
        assert_eq!(d.clone().validated(&h).unwrap().freq.start, Some(4));
        d.freq.start = Some(7);
        assert!(d.clone().validated(&h).is_err());
        let mut daily = data("daily", None);
        daily.freq.start = Some(2);
        assert_eq!(daily.validated(&h).unwrap().freq.start, None);
    }
}
