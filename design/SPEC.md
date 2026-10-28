# Favor — functional spec

A chore manager for a shared flat. Each roommate has an account. Chores are tasks with a difficulty and frequency. Roommates trade **favor** by taking on each other's tasks. The design reference is `Favor.dc.html` (Industry design system: Barlow Condensed / Barlow, steel accent #5980a6, square hairline "blueprint" frames with corner marks).

## Core concepts

### Users
- One account per roommate (name, login).
- A household has N roommates (the design assumes 3: Marta, Leo, Inés).

### Tasks
| Field | Values |
| --- | --- |
| name | string |
| difficulty | `easy` = 1, `medium` = 2, `hard` = 3 (this is also its **value** in favor) |
| frequency | `daily`, `weekly`, or `every N days` (N ≥ 2) |
| assignment | `rotate`: the person responsible changes each time, cycling through roommates in a fixed order. `fixed`: always the same person |
| assignee | for rotate, who's up next; for fixed, the owner |

### Task instances ("today items")
A task due on a given day creates an **instance**: `{taskId, assignee, status, handTo?, reason?, daysLeft?}`.
- `status`: `open` | `done` | `handoff` (waiting on recipient) | `cantdo` (waiting on approval) | `excused`
- Daily and every-N tasks must be done **that day**.
- Weekly tasks have a **window of 7 days** (`daysLeft`). They show "N days left this week" and stay listed until the window ends.

### Favor balances
- Directional, per pair: `bal[A][B]` = favor A holds **on** B, which A can spend on B.
- **Floor at zero.** Balances never go negative.
- **Earn:** when B accepts a task handed over by A, `bal[B][A] += value`.
- **Spend:** A can hand a task to B without consent if `bal[A][B] >= value`; then `bal[A][B] -= value`.
- **Lose:** if a task instance ends unresolved (see Day rollover), the assignee loses `value` with **every** other roommate: `bal[assignee][X] = max(0, bal[assignee][X] - value)`. Record how much was actually lost per person, since the floor can make it less than `value`; refunds need this.

## Screens

### Login
- Pick your account, enter password, sign in.
- "Switch" in the header logs out.

### Header (all app screens)
- Tabs: Today, Editor, Favors, Activity.
- Badges: Today shows the count of items in "Needs you". Editor shows the count of proposals you haven't voted on.
- Current user and a Switch/logout button.
- Demo only: "Skip to midnight" triggers the day rollover. In production this runs on a scheduler at local midnight.

### Today
**Header:** date, "Your tasks, {name}", progress (done+excused / total). The subtitle reads "{n} to go. Skip them and you lose up to {sum of values} favor with each roommate."

**Holiday control** (under the header):
- "Go on holiday" opens a modal with an optional note. Submitting creates a holiday request with status `requested`.
- While requested: show "Waiting for a roommate to approve" and a Withdraw button.
- **Any one** roommate can approve or decline it (from their Needs you panel).
- On approval (status `away`, record start day):
  - Your open/handoff/cantdo instances for today: **rotating** tasks are reassigned to the next roommate who isn't away; **fixed** tasks become `excused`.
  - Rotating tasks whose `assignee` is you advance to the next non-away roommate.
  - While away: the rotation skips you, fixed tasks you own are not generated, and you get no penalties.
  - The task list is replaced by a banner, "You're off the rota", with an **I'm back** button.
- "I'm back" clears the away status and immediately generates any of your fixed tasks that are due.
- Others see "On holiday" next to your name in their favor list.

**Your tasks:** one card per instance assigned to you.
- Shows difficulty pips (1–3 filled squares), the difficulty label and value, the name, and meta: frequency · Rotates/Always yours · due text.
- Actions when `open`:
  - **Done**: set status to `done`. Can be undone the same day.
  - **Can't do**: opens a modal that requires a reason. Sets status to `cantdo`. **Any one** other roommate can Excuse it (status `excused`, no penalty) or Deny it (back to `open`). The assignee can Withdraw.
  - **Hand off**: opens a modal listing each other roommate with:
    - **Ask nicely**: status `handoff`, `handTo` = recipient. The recipient sees it in Needs you. **Take it** moves the assignee to the recipient (status `open`) and gives `bal[recipient][sender] += value`. **Nope** returns it to `open` with the sender. The sender can Withdraw.
    - **Spend {value}**: only enabled if `bal[me][them] >= value`. The task moves immediately and `bal[me][them] -= value`.
- Done and excused cards are dimmed; done cards show the name struck through.

**Missed** (only shown if you have any): tasks that ended unresolved in previous days.
- Shows the name, "Missed {day} · cost you {actual total lost} favor".
- **I did it, forgot to tick**: status `claimed`. Any one other roommate gets a "Forgot to tick" item in Needs you.
  - **Confirm**: refund exactly the recorded per-person losses to `bal[assignee][X]` and remove the item.
  - **Didn't see it**: status `denied`, and the item stays, marked "Not confirmed".
- **Fair enough**: dismisses the item, with no refund.

**Everyone else:** a table of the other roommates' instances today (task, who, difficulty, status).

**Sidebar: Needs you.** Actionable requests addressed to you:
- Hand-off requests to you (Take it / Nope)
- Can't-do requests from others (Excuse / Deny)
- Forgot-to-tick claims from others (Confirm / Didn't see it)
- Holiday requests from others (Approve / Not now)

If there's nothing, show an empty state.

**Sidebar: Your favor.** One row per roommate with "You hold {bal[me][them]}" and "They hold {bal[them][me]}", plus a link to Favors.

### Editor
- **Every change requires a unanimous vote.** Proposals are add, edit or delete.
- "Propose a task" opens a form: name, difficulty (segmented), frequency (Daily / Weekly / Every X days with a number input), assignment (Rotates / Always the same person), and "Starts with" (rotate) or "Responsible" (fixed) to pick a roommate. Submit sends it to the vote.
- Per-task Edit opens the same form prefilled and creates an edit proposal. Delete creates a delete proposal.
- The proposer automatically votes yes.
- While a task has a pending proposal, its Edit and Delete are disabled and it shows a "Vote pending" tag.
- **Pending changes queue** (top of Editor): one card per proposal.
  - Shows the kind, the proposer, the title, and the changes. For edits, show only changed fields as old (struck through) → new.
  - Shows each roommate's vote: Yes / No / Waiting.
  - If you haven't voted: **Approve** / **Veto**. Otherwise: "You said yes. Waiting on X and Y."
  - **Any veto discards the proposal.** When **all** roommates vote yes it is applied:
    - add: create the task, first due tomorrow.
    - edit: update its fields.
    - delete: remove the task and today's instances of it.
- **All tasks** table: name (+ Vote pending tag), difficulty, frequency, assignment ("Rotates · A → B → C" or "Always X"), up now (today's assignee, or "X, in N days"), and edit/delete actions.

### Favors
- Rules strip: Earn / Spend / Lose, one sentence each.
- One card per other roommate: "You & {name}", a big number for "You hold on {name}" and one for "{name} holds on you".
- "Between you two": the last 5 favor events involving both of you (takes, spends, misses), with deltas.

### Activity
A reverse-chronological log of everything: done, asks, accepts and declines, spends, can't-do requests and decisions, misses, refunds, votes, holiday events. Each entry has a day label (Today / Yesterday / date), the text, and an optional delta.

## Day rollover (runs at midnight)
For every task:
1. **No instance today:** `due = max(0, due - 1)`.
2. **Weekly instance with `daysLeft > 1`:** carry it over with `daysLeft - 1`, keeping its status.
3. **Otherwise the instance ends:**
   - If its status is `open`, `handoff` or `cantdo`, it is **missed**. Apply the loss to every other roommate (floor 0) and record the actual per-person loss. Create a Missed record `{taskId, assignee, day, lost, status:'missed'}` and log it.
   - Set `due`: weekly → 0 (a new week starts tomorrow); daily/every-N → `period - 1` (daily is 1).
   - If the task rotates, advance `assignee` to the next roommate who isn't away.
4. Create new instances for tasks with `due == 0` whose assignee isn't away and that have no carried-over instance. Weekly instances start with `daysLeft = 7`.

Holiday users are never penalised; their instances were reassigned or excused on approval.

## Suggested data model
- `users(id, name, password_hash)`
- `tasks(id, name, difficulty, freq_type, freq_n, mode, assignee_id, due_in_days, rotation_order)`
- `instances(id, task_id, date, assignee_id, status, hand_to_id, reason, days_left)`
- `balances(holder_id, debtor_id, amount ≥ 0)`
- `missed(id, task_id, assignee_id, date, lost_json, status)`
- `proposals(id, kind, task_id, data_json, proposer_id, created_at)` and `votes(proposal_id, user_id, yes)`
- `holidays(user_id, status, note, since)`
- `activity(id, date, kind, text, delta, user_ids)`

## Copy tone
Playful but brief, for example: "Nothing on your plate today. Suspicious, but enjoy it.", "Welcome back. The dishes missed you.", "Escaping the chores?"
