# Fly.io hosting — deploy runbook

Host mini-app-mcp on a Fly.io Machine: one always-on daemon (`--mcp-http`)
with SQLite + `schema.yaml` on a persistent volume, TLS at the Fly edge,
bearer-token auth, and a daily backup upload to S3-compatible storage via
supercronic. The single-writer model from the README's Multi-device mode is
preserved — every device talks to this one machine.

Config lives in `contrib/fly/fly.toml`; the repo-root `Dockerfile` builds
with `--features s3-upload` and bundles `mini-app-backup` + supercronic
(inert unless `BACKUP_CRON` is set).

## 0. Prerequisites

- `flyctl` installed and authenticated (`fly auth login`)
- An S3-compatible bucket for backups (optional but recommended; B2 works
  out of the box — see README "Snapshot upload")
- A bearer token of your choosing (e.g. `openssl rand -hex 32`)

## 1. Create app + volume

```sh
fly apps create <your-app-name>
fly volumes create mini_app_data --app <your-app-name> --region nrt --size 1
```

Then set `app = "<your-app-name>"` (and `primary_region` if not `nrt`) in
`contrib/fly/fly.toml`.

## 2. Secrets

Secrets become environment variables inside the machine — the same
`MINI_APP_*` contract as a local daemon.

```sh
# Generate the server key once and keep it — clients need the same value (§6).
TOKEN=$(openssl rand -hex 32) && echo "$TOKEN"

fly secrets set --app <your-app-name> \
  MINI_APP_HTTP_TOKEN=$TOKEN \
  MINI_APP_S3_ENDPOINT=https://s3.<region>.backblazeb2.com \
  MINI_APP_S3_BUCKET=<bucket> \
  MINI_APP_S3_ACCESS_KEY_ID=<key-id> \
  MINI_APP_S3_SECRET_ACCESS_KEY=<key>
```

`MINI_APP_HTTP_TOKEN` is **required**: the server refuses to start on a
non-loopback bind (`0.0.0.0:8484` inside the container) without it. The
`MINI_APP_S3_*` group is only needed for backup upload; without it the
cron run fails with `UPLOAD_NOT_CONFIGURED` (visible in `fly logs`).

## 3. Deploy

From the repo root (the Dockerfile is auto-detected there):

```sh
fly deploy --config contrib/fly/fly.toml
```

## 4. Smoke

```sh
APP=https://<your-app-name>.fly.dev
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
# no token → 401
curl -s -o /dev/null -w 'no-auth:%{http_code}\n' -X POST "$APP/mcp" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT"
# token → 200
curl -s -o /dev/null -w 'good:%{http_code}\n' -X POST "$APP/mcp" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT"
```

**Expect**: `no-auth:401` / `good:200`. `fly logs` shows
`serving MCP over streamable HTTP at /mcp ... auth="bearer-token"`.

## 5. Data migration (existing local `~/.mini-app/` → volume)

Do this **before** pointing any client at the app, then no writes can race
the swap. Archive creation follows `docs/runbooks/data-migration.md` §2
(stop the local daemon first so WAL/SHM are quiescent):

```sh
# local host
tar -czf /tmp/mini-app-data.tar.gz -C ~ .mini-app
fly ssh sftp shell --app <your-app-name>
>> put /tmp/mini-app-data.tar.gz /data/incoming.tar.gz
```

Swap on the machine, then restart so the daemon reopens the new files:

```sh
fly ssh console --app <your-app-name>
# mv the auto-created fresh dir aside, then unpack (tar contains `.mini-app/`)
mv /data/mini-app /data/mini-app.fresh
mkdir -p /data/mini-app && tar -xzf /data/incoming.tar.gz -C /data/mini-app --strip-components=1
rm /data/incoming.tar.gz && exit
fly machine restart --app <your-app-name>
```

Verify with a `tools/call list` against a known table (same curl shape as
§4 plus `Mcp-Session-Id`, see `docs/runbooks/multi-device-smoke.md` §1),
then remove `/data/mini-app.fresh`.

## 6. Client registration

```sh
claude mcp add --transport http mini-app https://<your-app-name>.fly.dev/mcp \
  --header "Authorization: Bearer <token>"
```

Register under a distinct name if a local stdio `mini-app` entry exists.

## 7. Backup cron

`BACKUP_CRON` in `contrib/fly/fly.toml` (default `0 3 * * *`, UTC) drives
supercronic → `mini-app-backup`, which calls `data_snapshot(upload=true)`
on the local endpoint and exits non-zero when `upload_errors[]` is
non-empty — failures land in `fly logs` with a `mini-app-backup:` prefix.

Manual check from inside the machine:

```sh
fly ssh console --app <your-app-name> -C "mini-app-backup --dry-run"
# → mini-app-backup: dry-run ok — upload config valid, N table(s) in scope
```

## 8. Notes / constraints

- **Exactly one machine.** The volume (and the SQLite single-writer model)
  belongs to one machine; do not scale out (`fly scale count 1`), and keep
  `auto_stop_machines = "off"`.
- **Remote retention** is the bucket's job (e.g. B2 lifecycle rules) — the
  server never deletes uploaded snapshots (KNOWN LIMITATION, see README).
- **Fly volume snapshots** (automatic daily, ~5-day retention) are an
  extra safety net under the app-level B2 uploads, not a replacement.
- Local snapshot generations written to `_snapshots/` on the volume are
  pruned per `schema.yaml` `dump.keep` as usual; volume sizing should
  account for them.
