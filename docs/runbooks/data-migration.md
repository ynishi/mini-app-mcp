# Data migration — moving an existing data dir to the central daemon host

Procedure for migrating existing mini-app data (the user-tables dir,
`~/.mini-app/`) from one host to the central `--mcp-http` daemon host with
plain `tar`. Project-tables dirs (`./.mini-app/`) travel with their repo and
are out of scope. Full cycle (place data → tear down → clean start →
restore → recovery) verified on Linux (2026-08-13, v0.17.0).

## 0. Data layout

```
~/.mini-app/
├── _global.db (+ -wal / -shm)     # global metadata
├── _backup/                        # automatic backups from schema_update/delete
├── _snapshots/                     # data_snapshot output
└── <table>/                        # 1 table = 1 self-contained dir
    ├── <table>.db (+ -wal / -shm)
    └── schema.yaml
```

- Each table dir is self-contained, so a **single table can be migrated by
  copying its one dir** (see §5).
- `-wal` / `-shm` are SQLite WAL sidecars. Copying them is safe **only while
  no process has the DB open** — SQLite recovers from the WAL on next open.
  Never tar a live data dir.

## 1. Source host: stop every server and client

```sh
systemctl --user stop mini-app-mcp    # Linux daemon, if installed
# macOS daemon, if installed:
#   launchctl bootout gui/$(id -u)/com.mini-app-mcp
pgrep -fl mini-app-mcp
```

**Expect**: no `mini-app-mcp` process remains. stdio instances are spawned by
MCP clients (e.g. editor/agent sessions) — close those sessions too. A
process that stays open across the move keeps file handles on the old
inodes and its writes will be lost after restore.

## 2. Source host: archive

```sh
tar -C ~ -czf /tmp/mini-app-data.tgz .mini-app
```

## 3. Transfer

```sh
scp /tmp/mini-app-data.tgz <central-host>:/tmp/
```

## 4. Central host: stop, set aside, extract, start

```sh
systemctl --user stop mini-app-mcp
mv ~/.mini-app ~/.mini-app.bak.$(date +%Y%m%d)   # keep, do not delete
tar -C ~ -xzf /tmp/mini-app-data.tgz
systemctl --user start mini-app-mcp
```

The `mv` step is required even on a host that never held data: the daemon
**auto-creates a fresh `~/.mini-app` (with a new `_global.db`) on clean
start**, and extracting over that mixed state is asking for confusion.
Always set the existing dir aside first.

Verify:

```sh
systemctl --user is-active mini-app-mcp
curl -s -o /dev/null -w "%{http_code}\n" -X POST http://127.0.0.1:8484/mcp \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"migrate","version":"0"}}}'
```

**Expect**: `active` and `200`. Then `list` a known table from any MCP client
and confirm the migrated rows are present.

## 5. Single-table variant

```sh
# source (after §1 stop)
tar -C ~/.mini-app -czf /tmp/<table>.tgz <table>
# central host: stop daemon → set aside any same-named dir → extract → start
tar -C ~/.mini-app -xzf /tmp/<table>.tgz
```

The daemon discovers tables at startup; on a running daemon the `reload`
tool works too.

## 6. Post-migration checks and caveats

- **Ephemeral sidecars**: after the daemon reopens a DB, its `-shm` file
  differs from the archived copy. That is expected — `-shm` is regenerated
  shared memory, not data.
- **Symlinked `schema.yaml`**: a table's `schema.yaml` may be a symlink to an
  absolute path on the source host and will dangle after migration. Check
  with:

  ```sh
  find ~/.mini-app -type l ! -exec test -e {} \; -print
  ```

  Replace any hit with a real file (copy the target from the source host) or
  re-point it to a path that exists on the central host.
- **Old host becomes a client**: after migration the source host should talk
  to the central daemon over HTTP only. Do not leave a config that reads the
  local `~/.mini-app` — two writable copies means two diverging sources of
  truth.
- **No row-level merge**: this procedure replaces dirs wholesale. If both
  sides accumulated distinct new rows in the same table, merging them needs
  manual work (`data_snapshot` / `sqlite3 .dump`) — there is no merge
  subcommand. The `.bak` dir from §4 is your safety net; keep it until the
  migration is confirmed.
