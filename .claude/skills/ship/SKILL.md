---
name: ship
description: Commit the working tree, push to origin, and deploy Favor to the home server (serujiserver) with deploy/deploy.sh, then verify it's live. Use only when the user runs /ship or explicitly asks to commit, push and deploy.
disable-model-invocation: true
---

# Ship Favor: commit → push → deploy → verify

Running this skill is the user's go-ahead for all three steps. Stop and ask
only when something below says to.

Work from the repo root (`favor-task-manager/`).

## 1. Review what will be committed

```sh
git status --short --untracked-files=all
git branch --show-current
git fetch -q origin && git status -sb | head -1
```

- Must be on `main`. If not, stop and ask.
- If `main` is behind `origin/main`, stop and ask (don't pull or rebase on your own).
- Never commit `.env`, `dev.sh`, `target/`, or anything holding passwords or
  keys. They're gitignored; if one shows up anyway, stop and say so.
- Look at `git diff` for anything unexpected (debug code, stray files, files
  unrelated to this session's work). Mention it, and leave unrelated files out
  unless the user said to include everything.
- If there's nothing to commit and HEAD is already pushed, skip to step 3.

## 2. Commit and push

Match the repo's message style: a past-tense summary line ("Added …",
"Changed …", "Removed …"), then a short body saying what changed and why if
it isn't obvious. Stage files by name, not `git add -A`.

```sh
git add <files> && git commit -F - <<'EOF'
<Summary line>

<Optional body>

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
git push origin main
```

If the push is rejected, stop and report; never force-push.

## 3. Deploy

The deploy uses the key in the local ssh-agent (systemd `ssh-agent.socket`):

```sh
export SSH_AUTH_SOCK=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/ssh-agent.socket
ssh-add -l >/dev/null || echo "no key loaded"
```

If no key is loaded, ask the user to run `! ssh-add ~/.ssh/id_aboies` (it
needs their passphrase) and wait.

Run the deploy in the background. A backend change rebuilds the image on the
server, which takes a few minutes; static-only changes sync in seconds.

```sh
deploy/deploy.sh 2>&1 | grep -v -E "^#[0-9]+ (sha256|extracting|\[internal\])|Downloading|Downloaded|Compiling [a-z]" | tail -40
```

Never use `FORCE=1` unless the user asks. If it fails, show the relevant
output and stop. Never touch the server's `.env`: the user manages it by hand.

## 4. Verify

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://favor.seruji.net/healthz
for f in index.html admin.html; do curl -s https://favor.seruji.net/$f | cmp - backend/static/$f && echo "$f matches"; done
ssh serujiserver 'cd /srv/data/docker_arcane/projects/Favor && cat REVISION && docker compose logs --since 10m app 2>&1 | grep -iE "error|warn|panic" | tail -5'
```

`REVISION` must show the commit just pushed.

## 5. Report

Briefly: the commit hash and summary, whether the server rebuilt or only
synced pages, and that https://favor.seruji.net is up on that commit. List any
errors or warnings from the logs, and anything that was skipped or left
uncommitted.
