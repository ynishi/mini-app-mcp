#!/bin/sh
# mini-app-backup.sh — trigger `data_snapshot(upload=true)` on a running
# mini-app-mcp HTTP daemon (`--mcp-http`) and fail loudly when anything
# went wrong. Designed to sit behind cron / supercronic: all diagnostics
# go to stderr, the exit code carries the verdict.
#
# Environment:
#   MINI_APP_URL         MCP endpoint (default: http://127.0.0.1:8484/mcp)
#   MINI_APP_HTTP_TOKEN  bearer token; required when the daemon enforces auth
#
# Usage:
#   mini-app-backup.sh [--dry-run] [--table NAME] [--scope user|project]
#
#   --dry-run   validate upload configuration and report affected tables
#               without writing snapshots or uploading anything
#
# Exit codes:
#   0  snapshots written, upload_errors empty
#   1  transport / protocol failure (curl error, non-200, no session id)
#   2  tool error (e.g. UPLOAD_NOT_CONFIGURED — missing MINI_APP_S3_* env
#      on the daemon, or a binary built without the s3-upload feature)
#   3  snapshots written but one or more uploads failed (upload_errors[])
#
# Dependencies: curl, jq

set -u

URL="${MINI_APP_URL:-http://127.0.0.1:8484/mcp}"

DRY=false
TABLE=""
SCOPE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY=true ;;
        --table) TABLE="$2"; shift ;;
        --scope) SCOPE="$2"; shift ;;
        *) echo "mini-app-backup: unknown argument: $1" >&2; exit 1 ;;
    esac
    shift
done

for dep in curl jq; do
    command -v "$dep" >/dev/null 2>&1 || {
        echo "mini-app-backup: missing dependency: $dep" >&2
        exit 1
    }
done

# curl -H with an empty value drops the header, so the no-token path can
# share the same invocation shape as the bearer path.
if [ -n "${MINI_APP_HTTP_TOKEN:-}" ]; then
    AUTH="Authorization: Bearer $MINI_APP_HTTP_TOKEN"
else
    AUTH="Authorization;"
fi

ARGS=$(jq -cn --arg table "$TABLE" --arg scope "$SCOPE" --argjson dry "$DRY" '
    {upload: true}
    + (if $dry then {dry_run: true} else {} end)
    + (if $table != "" then {table: $table} else {} end)
    + (if $scope != "" then {scope: $scope} else {} end)')

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"mini-app-backup","version":"1"}}}'

# --- 1. initialize: capture the mcp-session-id response header ------------
HEADERS=$(curl -sS -D - -o /dev/null -X POST "$URL" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "$AUTH" \
    -d "$INIT") || { echo "mini-app-backup: initialize request failed (curl) url=$URL" >&2; exit 1; }

STATUS=$(printf '%s\n' "$HEADERS" | head -n 1 | awk '{print $2}')
if [ "$STATUS" != "200" ]; then
    echo "mini-app-backup: initialize returned HTTP $STATUS (url=$URL)" >&2
    [ "$STATUS" = "401" ] && echo "mini-app-backup: hint: check MINI_APP_HTTP_TOKEN" >&2
    exit 1
fi

SID=$(printf '%s\n' "$HEADERS" | grep -i '^mcp-session-id:' | tr -d '\r' | awk '{print $2}')
if [ -z "$SID" ]; then
    echo "mini-app-backup: no mcp-session-id header in initialize response" >&2
    exit 1
fi

# --- 2. notifications/initialized ----------------------------------------
curl -sS -o /dev/null -X POST "$URL" \
    -H "Mcp-Session-Id: $SID" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "$AUTH" \
    -d '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    || { echo "mini-app-backup: notifications/initialized failed" >&2; exit 1; }

# --- 3. tools/call data_snapshot ------------------------------------------
BODY=$(curl -sS -X POST "$URL" \
    -H "Mcp-Session-Id: $SID" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "$AUTH" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"data_snapshot\",\"arguments\":$ARGS}}") \
    || { echo "mini-app-backup: tools/call request failed (curl)" >&2; exit 1; }

# Streamable HTTP answers as SSE (`data: {...}` lines) or plain JSON.
JSON=$(printf '%s\n' "$BODY" | sed -n 's/^data: //p' | tail -n 1)
[ -n "$JSON" ] || JSON="$BODY"

RPC_ERR=$(printf '%s' "$JSON" | jq -r '.error.message // empty' 2>/dev/null)
if [ -n "$RPC_ERR" ]; then
    echo "mini-app-backup: JSON-RPC error: $RPC_ERR" >&2
    exit 2
fi

TEXT=$(printf '%s' "$JSON" | jq -r '.result.content[0].text // empty' 2>/dev/null)
if [ -z "$TEXT" ]; then
    echo "mini-app-backup: unexpected response shape:" >&2
    printf '%s\n' "$JSON" >&2
    exit 1
fi

IS_ERR=$(printf '%s' "$JSON" | jq -r '.result.isError // false' 2>/dev/null)
if [ "$IS_ERR" = "true" ]; then
    echo "mini-app-backup: tool error: $TEXT" >&2
    exit 2
fi

UPLOAD_ERR_COUNT=$(printf '%s' "$TEXT" | jq -r '(.upload_errors // []) | length' 2>/dev/null)
if [ -z "$UPLOAD_ERR_COUNT" ]; then
    echo "mini-app-backup: could not parse tool result:" >&2
    printf '%s\n' "$TEXT" >&2
    exit 1
fi

if [ "$UPLOAD_ERR_COUNT" -gt 0 ]; then
    echo "mini-app-backup: $UPLOAD_ERR_COUNT upload(s) failed:" >&2
    printf '%s' "$TEXT" | jq -r '.upload_errors[] | "  \(.table // "?"): \(.error // .)"' >&2
    exit 3
fi

# Success summary on stdout (supercronic/cron mail or log capture).
printf '%s' "$TEXT" | jq -r '
    if .dry_run == true then
        "mini-app-backup: dry-run ok — upload config valid, \((.affects.target_tables // []) | length) table(s) in scope"
    else
        "mini-app-backup: ok — \((.uploaded // []) | length) snapshot(s) uploaded" ,
        ((.uploaded // [])[] | "  \(.key) (\(.bytes) bytes)")
    end'
exit 0
