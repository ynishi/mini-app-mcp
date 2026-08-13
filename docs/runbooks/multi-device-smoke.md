# Multi-device HTTP transport — smoke test runbook

End-to-end verification of the streamable HTTP transport (`--mcp-http`) on a
real host. Every step is local-only — nothing is exposed beyond loopback
unless a test explicitly says so. Verified on Linux and macOS (2026-08-13,
v0.16.0).

## 0. Prerequisites

```sh
git pull && cargo install --path crates/mcp
mini-app-mcp --version
```

Create a throwaway test table (any scratch directory):

```sh
mkdir -p /tmp/mas/.mini-app/notes
printf 'table: notes\nfields:\n  - name: title\n    type: string\n    required: true\n' > /tmp/mas/.mini-app/notes/schema.yaml
cd /tmp/mas
```

Shared shell fragments used below:

```sh
ENV='MINI_APP_PROJECT_DIR=./.mini-app MINI_APP_USER_DIR=./nouser'
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
HDR=(-H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')
```

> **Note on process cleanup**: `kill %1` does not work in non-interactive
> shells (no job control). Always capture the server pid with `SRV=$!` and
> `kill $SRV`, and finish with the residual-listener check in §7.

## 1. HTTP consumption path (initialize → create → list)

```sh
env $ENV mini-app-mcp --mcp-http --bind 127.0.0.1:8490 2>server.err &
SRV=$!
sleep 1
SID=$(curl -s -D - -o /dev/null -X POST http://127.0.0.1:8490/mcp "${HDR[@]}" -d "$INIT" | grep -i mcp-session-id | tr -d '\r' | awk '{print $2}')
curl -s -X POST http://127.0.0.1:8490/mcp -H "Mcp-Session-Id: $SID" "${HDR[@]}" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}' > /dev/null
curl -s -X POST http://127.0.0.1:8490/mcp -H "Mcp-Session-Id: $SID" "${HDR[@]}" -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"create","arguments":{"table":"notes","data":{"title":"smoke"}}}}'
curl -s -X POST http://127.0.0.1:8490/mcp -H "Mcp-Session-Id: $SID" "${HDR[@]}" -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list","arguments":{"table":"notes"}}}'
kill $SRV
```

**Expect**: initialize returns 200 with an `mcp-session-id` header; `create`
returns the row JSON; `list` contains that row.

## 2. Startup log

```sh
grep "serving MCP over streamable HTTP" server.err
```

**Expect**: an `INFO ... serving MCP over streamable HTTP at /mcp
addr=127.0.0.1:8490 auth="loopback"` line, free of ANSI escapes.

Logs go to **stderr** by design (stdout is the MCP protocol channel in stdio
mode). Under launchd this lands in `StandardErrorPath` (`.err`); an empty
`StandardOutPath` (`.log`) is expected. Under systemd, use `journalctl`.

## 3. Bearer auth (401 / 200)

Loopback bind — nothing is exposed off-host.

```sh
env $ENV MINI_APP_HTTP_TOKEN=tok mini-app-mcp --mcp-http --bind 127.0.0.1:8491 2>/dev/null &
SRV=$!
sleep 1
curl -s -o /dev/null -w 'no-auth:%{http_code}\n'                              -X POST http://127.0.0.1:8491/mcp "${HDR[@]}" -d "$INIT"
curl -s -o /dev/null -w 'bad:%{http_code}\n'  -H 'Authorization: Bearer bad'  -X POST http://127.0.0.1:8491/mcp "${HDR[@]}" -d "$INIT"
curl -s -o /dev/null -w 'good:%{http_code}\n' -H 'Authorization: Bearer tok'  -X POST http://127.0.0.1:8491/mcp "${HDR[@]}" -d "$INIT"
kill $SRV
```

**Expect**: `no-auth:401` / `bad:401` / `good:200`.

## 4. Non-loopback bind refusal

This guard fires **before** any listener is opened (startup bails ahead of
`TcpListener::bind`), so the test exposes nothing.

```sh
env $ENV mini-app-mcp --mcp-http --bind 0.0.0.0:8492; echo "exit=$?"
```

**Expect**: `Error: refusing to bind non-loopback address 0.0.0.0:8492
without MINI_APP_HTTP_TOKEN ...` and `exit=1`, with no listener.

## 5. Stdio regression (no client restart required)

Verifying the binary's stdio transport only needs a direct spawn. Restarting
an MCP client is a separate concern (swapping the client's live server
process) and is not part of this check.

```sh
echo "$INIT" | env $ENV mini-app-mcp --mcp | head -c 80
```

**Expect**: stdout starts with `{"jsonrpc":"2.0","id":1,"result":{...` and
contains no log lines (logs are on stderr).

## 6. Daemon form

### Linux (systemd user unit)

```sh
cp contrib/systemd/mini-app-mcp.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user start mini-app-mcp
systemctl --user is-active mini-app-mcp                     # → active
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:8484/mcp "${HDR[@]}" -d "$INIT"   # → 200
journalctl --user -u mini-app-mcp | grep "serving MCP"      # same check as §2
# Teardown
systemctl --user stop mini-app-mcp && rm ~/.config/systemd/user/mini-app-mcp.service && systemctl --user daemon-reload
```

### macOS (launchd user agent)

```sh
sed "s|__HOME__|$HOME|g" contrib/launchd/io.github.ynishi.mini-app-mcp.plist > ~/Library/LaunchAgents/io.github.ynishi.mini-app-mcp.plist
plutil -lint ~/Library/LaunchAgents/io.github.ynishi.mini-app-mcp.plist      # → OK
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.ynishi.mini-app-mcp.plist
lsof -nP -iTCP:8484 | grep LISTEN                            # listener present
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:8484/mcp "${HDR[@]}" -d "$INIT"   # → 200
grep "serving MCP" /tmp/mini-app-mcp.err                     # log lands in .err; empty .log is expected
# Teardown
launchctl bootout gui/$(id -u)/io.github.ynishi.mini-app-mcp
rm ~/Library/LaunchAgents/io.github.ynishi.mini-app-mcp.plist
```

## 7. Residual-listener check (always run last)

```sh
pgrep -f "mini-app-mcp --mcp-http" || echo "no residual listeners"
```

**Expect**: `no residual listeners`. If pids are printed, kill them
explicitly — leaked test servers keep ports 8490/8491 occupied and skew the
next run.

## Reporting

List all eight items (§1–§5, the §6 variant for your OS, §7) as a strict
`[OK]` / `[NG]` two-value checklist. An item that was not actually invoked is
`[NG]`, never "skipped" — an unexercised path is an unverified path.
