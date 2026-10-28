-- In-app notifications, also delivered as browser push.
CREATE TABLE notifications (
    id         BIGSERIAL PRIMARY KEY,
    user_id    INT  NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,                  -- 'vote' | 'handoff'
    title      TEXT NOT NULL,
    body       TEXT NOT NULL DEFAULT '',
    link       TEXT NOT NULL DEFAULT 'today',  -- tab to open
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    read_at    TIMESTAMPTZ,
    pushed_at  TIMESTAMPTZ
);
CREATE INDEX notifications_by_user ON notifications (user_id, id DESC);
CREATE INDEX notifications_unpushed ON notifications (id) WHERE pushed_at IS NULL;

-- One row per browser/device that allowed notifications.
CREATE TABLE push_subscriptions (
    endpoint   TEXT PRIMARY KEY,
    user_id    INT  NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    p256dh     TEXT NOT NULL,
    auth       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Server-generated values that must survive restarts (e.g. the VAPID key).
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
