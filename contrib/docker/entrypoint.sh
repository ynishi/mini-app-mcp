#!/bin/sh
# Container entrypoint. Behaves exactly like running `mini-app-mcp` directly
# unless BACKUP_CRON is set, in which case supercronic is started alongside
# the server to run scheduled backups (contrib/backup/mini-app-backup.sh)
# against the local HTTP endpoint.
#
#   BACKUP_CRON     cron expression, e.g. "0 3 * * *" (unset = no cron)
#   BACKUP_ARGS     extra args for mini-app-backup (e.g. "--scope user")
#
# supercronic runs in the background; the server is exec'd as the main
# process so container lifecycle (signals, exit code) follows the server.
# Note: if supercronic itself dies, backups stop silently until the machine
# restarts — check logs (`supercronic` prefix) when in doubt.
set -eu

if [ -n "${BACKUP_CRON:-}" ]; then
    printf '%s /usr/local/bin/mini-app-backup %s\n' \
        "$BACKUP_CRON" "${BACKUP_ARGS:-}" > /tmp/mini-app-crontab
    supercronic -passthrough-logs /tmp/mini-app-crontab &
fi

exec mini-app-mcp "$@"
