# Favor

A chore manager for a shared flat. Roommates trade **favor** by taking on each
other's tasks. The UI is the Claude Design export in
[`design/Favor.dc.html`](design/Favor.dc.html).

Two containers:

| Service | What |
| --- | --- |
| `app` | Rust (axum + sqlx) backend. Serves the page and the JSON API, and runs the midnight rollover. |
| `db`  | PostgreSQL 17. Holds everything, in the `favor-db` volume. |

## Deploy with Portainer

1. **Stacks → Add stack**.
2. Pick one:
   - **Repository**: point it at this repo, compose path `docker-compose.yml`.
     Portainer builds the image from `backend/`.
   - **Web editor**: paste `docker-compose.yml`. The image is pulled from
     `ghcr.io/aobies59/favor:latest`, which the GitHub Action publishes on every
     push to `main`. Make the package public on GitHub (or add the registry
     to Portainer) so the server can pull it.
3. Under **Environment variables**, set at least:

   | Variable | Example | Notes |
   | --- | --- | --- |
   | `DB_PASSWORD` | `s3cretLongPassword` | Required. Letters and digits only, since it goes into a URL. |
   | `FAVOR_ADMIN_PASSWORD` | `another-long-one` | Required. Signs you in to `/admin`. |
   | `FAVOR_USERS` | `Marta:pw1,Leo:pw2,Inés:pw3` | Optional. Accounts to create on the very first start. |
   | `FAVOR_HOUSE_NAME` | `Flat 3B` | Shown on the login screen. |
   | `FAVOR_TZ` | `Europe/Madrid` | When "midnight" happens. |
   | `FAVOR_PORT` | `8080` | Host port. |
   | `FAVOR_COOKIE_SECURE` | `true` | Set when served over HTTPS. |

4. Deploy, then open `http://<server>:8080`.

## Managing roommates

Accounts live in the database. Open `http://<server>:8080/admin` and sign in
with `FAVOR_ADMIN_PASSWORD` to:

- **Add** a roommate. They join the end of the rotation.
- **Rename** someone. Past Activity entries keep the old name.
- **Set a new password.** It signs that person out on every device.
- **Reorder** with the arrows. The list order is the rotation order.
- **Remove** someone. Their tasks and today's instances pass to the next
  person in the rotation. Their balances, missed tasks and proposals are
  deleted, and any pending vote that's now unanimous is applied.

`FAVOR_USERS` only seeds accounts when the database has none. After that it's
ignored, so changing it does nothing.

Admin sessions last 12 hours. A wrong admin password takes a second to reject,
to slow down guessing. If you expose the app to the internet, serve it over
HTTPS and set `FAVOR_COOKIE_SECURE=true`.

## Run locally

```sh
cp .env.example .env   # edit it
docker compose up -d --build
```

Without Docker, start a Postgres and run:

```sh
cd backend
DATABASE_URL=postgres://favor:pw@localhost/favor \
FAVOR_ADMIN_PASSWORD=admin FAVOR_USERS='Marta:pw,Leo:pw,Inés:pw' \
cargo run
```

## How it works

- Every write runs in one Postgres transaction behind an advisory lock, so
  two roommates clicking at once can't break a balance.
- Each action returns the whole household snapshot (`GET /api/state` has the
  same shape). The page also re-polls every 15 s, so other people's changes show up.
- At local midnight the rollover ends the day: unfinished tasks are missed and
  cost favor, weekly tasks carry over until their 7-day window ends, rotating
  tasks move to the next person, and the new day's tasks are dealt. A scheduler
  checks every 30 s and catches up any midnights missed while the server was down.
- Sessions are HttpOnly cookies. Passwords are hashed with Argon2.
