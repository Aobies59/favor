-- Sessions for the admin page (signed in with FAVOR_ADMIN_PASSWORD).
CREATE TABLE admin_sessions (
    token      TEXT PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
