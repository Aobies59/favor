#!/usr/bin/env bash
# Deploy the current commit to the home server: deploy/deploy.sh   (FORCE=1 skips the git checks)
set -euo pipefail

# Use the systemd ssh-agent (ssh-agent.socket) so terminal and Claude Code sessions share one agent
export SSH_AUTH_SOCK="${SSH_AUTH_SOCK:-${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/ssh-agent.socket}"

HOST="serujiserver"
REMOTE="/srv/data/docker_arcane/projects/Favor"
CONTAINER="favor-app-1"
URL="http://10.36.9.10:3018"
SSH_OPTS="-o StrictHostKeyChecking=accept-new"
SSH="ssh $SSH_OPTS $HOST"
RSYNC="rsync -a --delete -e \"ssh $SSH_OPTS\""

# Baked into the image: any change here means a rebuild on the server
BUILD_INPUTS="Cargo.toml Cargo.lock Dockerfile .dockerignore migrations src"
# Bind-mounted into the container: syncing it is enough, no restart
STATIC_DIR="backend/static"

cd "$(dirname "$0")/.."

# Only deploy committed, pushed code so the server always matches a git commit.
if [[ "${FORCE:-0}" != 1 ]]; then
    if [[ -n "$(git status --porcelain)" ]]; then
        echo "✗ Uncommitted changes — commit first (or FORCE=1 to deploy anyway):" >&2
        git status --short >&2
        exit 1
    fi
    git fetch --quiet origin
    if [[ -z "$(git branch -r --contains HEAD)" ]]; then
        echo "✗ HEAD is not pushed to origin — push first (or FORCE=1 to deploy anyway)." >&2
        exit 1
    fi
fi

REV="$(git rev-parse --short HEAD) $(git rev-parse --abbrev-ref HEAD)"
[[ -n "$(git status --porcelain)" ]] && REV="$REV +dirty"

# The server's .env is created by hand and never leaves the server.
# static/ is created here (as us) so Docker doesn't create the bind mount as root.
ENV_STATE=$($SSH "mkdir -p $REMOTE/backend/static && cd $REMOTE && if [ -f .env ]; then { [ -O .env ] && chmod 600 .env; } || true;
    grep -q '^DB_PASSWORD=.' .env && grep -q '^FAVOR_ADMIN_PASSWORD=.' .env && echo ok || echo incomplete; else echo missing; fi")
if [[ "$ENV_STATE" != ok ]]; then
    cat >&2 <<EOF
✗ $HOST:$REMOTE/.env is $ENV_STATE. Create it on the server (chmod 600) with at least:

    DB_PASSWORD=<letters and digits only, it goes into a URL>
    FAVOR_ADMIN_PASSWORD=<password for /admin>

  Optional: FAVOR_HOUSE_NAME, FAVOR_TZ (default Europe/Madrid),
  FAVOR_COOKIE_SECURE (true behind HTTPS), FAVOR_USERS (Name:password,... seeds an empty database).
EOF
    exit 1
fi

# Rebuild only if the build inputs (or the compose file) differ from the last successful build
LOCAL_HASH=$( { (cd backend && find $BUILD_INPUTS -type f -print0 | sort -z | xargs -0 sha256sum)
                sha256sum deploy/compose.server.yaml; } | sha256sum | cut -d' ' -f1)
REMOTE_HASH=$($SSH "cat $REMOTE/.build-hash 2>/dev/null" || true)
[[ "$LOCAL_HASH" == "$REMOTE_HASH" ]] && NEED_REBUILD=false || NEED_REBUILD=true

echo "→ Deploying $REV"
if $NEED_REBUILD; then
    echo "→ Build files changed — syncing source and rebuilding the image on the server..."
    eval "$RSYNC $(printf 'backend/%s ' $BUILD_INPUTS) $HOST:$REMOTE/backend/"
    eval "$RSYNC deploy/compose.server.yaml $HOST:$REMOTE/compose.yaml"
    # Only the app is rebuilt and recreated; the database keeps running.
    $SSH "cd $REMOTE && docker compose up -d --build"
    $SSH "echo $LOCAL_HASH > $REMOTE/.build-hash"
fi

# Synced after a rebuild so a failed build never serves new pages against the old backend.
echo "→ Syncing $STATIC_DIR (live immediately, no restart)..."
eval "$RSYNC $STATIC_DIR $HOST:$REMOTE/backend/"
# Starts the stack if it isn't running; does nothing if it already is.
$NEED_REBUILD || $SSH "cd $REMOTE && docker compose up -d --no-build"

$SSH "echo '$REV $(date -Iseconds)' > $REMOTE/REVISION"

echo "→ Waiting for the app..."
for _ in $(seq 1 60); do
    if [[ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "$URL/healthz")" == 200 ]]; then
        $SSH "docker ps --filter name=$CONTAINER --format 'Status: {{.Status}}'"
        echo "✓ Done — $URL ($REV)"
        exit 0
    fi
    sleep 1
done
echo "✗ App did not respond within 60s — check: $SSH docker logs --tail 50 $CONTAINER" >&2
exit 1
