-- Favor schema. See SPEC.md for the rules these tables back.

CREATE TABLE users (
    id            SERIAL PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    -- Fixed rotation order for rotating tasks.
    position      INT  NOT NULL DEFAULT 0
);

CREATE TABLE sessions (
    token      TEXT PRIMARY KEY,
    user_id    INT  NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Single row holding the household's current logical day.
CREATE TABLE app_state (
    id    INT PRIMARY KEY CHECK (id = 1),
    today DATE NOT NULL
);

CREATE TABLE tasks (
    id          SERIAL PRIMARY KEY,
    name        TEXT NOT NULL,
    difficulty  TEXT NOT NULL CHECK (difficulty IN ('easy', 'medium', 'hard')),
    freq_type   TEXT NOT NULL CHECK (freq_type IN ('daily', 'weekly', 'every')),
    freq_n      INT  NOT NULL DEFAULT 1 CHECK (freq_n >= 1),
    mode        TEXT NOT NULL CHECK (mode IN ('rotate', 'fixed')),
    assignee_id INT  NOT NULL REFERENCES users(id),
    due_in_days INT  NOT NULL DEFAULT 0 CHECK (due_in_days >= 0)
);

CREATE TABLE instances (
    id          SERIAL PRIMARY KEY,
    task_id     INT  NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    date        DATE NOT NULL,
    assignee_id INT  NOT NULL REFERENCES users(id),
    status      TEXT NOT NULL CHECK (status IN ('open', 'done', 'handoff', 'cantdo', 'excused')),
    hand_to_id  INT  REFERENCES users(id),
    reason      TEXT,
    days_left   INT,
    UNIQUE (task_id)
);

CREATE TABLE balances (
    holder_id INT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    debtor_id INT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    amount    INT NOT NULL DEFAULT 0 CHECK (amount >= 0),
    PRIMARY KEY (holder_id, debtor_id)
);

CREATE TABLE missed (
    id          SERIAL PRIMARY KEY,
    task_id     INT  REFERENCES tasks(id) ON DELETE SET NULL,
    -- Copied so the record survives the task being deleted.
    task_name   TEXT NOT NULL,
    difficulty  TEXT NOT NULL,
    assignee_id INT  NOT NULL REFERENCES users(id),
    date        DATE NOT NULL,
    -- { "<user id>": favor actually lost with that user }
    lost_json   JSONB NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('missed', 'claimed', 'denied'))
);

CREATE TABLE proposals (
    id          SERIAL PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('add', 'edit', 'delete')),
    task_id     INT  REFERENCES tasks(id) ON DELETE CASCADE,
    data_json   JSONB,
    proposer_id INT  NOT NULL REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE votes (
    proposal_id INT  NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    user_id     INT  NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    yes         BOOLEAN NOT NULL,
    PRIMARY KEY (proposal_id, user_id)
);

CREATE TABLE holidays (
    user_id INT  PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    status  TEXT NOT NULL CHECK (status IN ('requested', 'away')),
    note    TEXT NOT NULL DEFAULT '',
    since   DATE
);

CREATE TABLE activity (
    id         BIGSERIAL PRIMARY KEY,
    date       DATE  NOT NULL,
    kind       TEXT  NOT NULL,
    text       TEXT  NOT NULL,
    delta      TEXT,
    user_ids   INT[] NOT NULL DEFAULT '{}',
    -- Whose event it is (e.g. who missed); used for the Favors history.
    actor_id   INT   REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX activity_recent ON activity (id DESC);
