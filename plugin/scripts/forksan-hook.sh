#!/bin/sh
# forksan hook shim: exec the persistent-data binary (it survives plugin
# updates), or kick off a background bootstrap when it's missing. A hook
# must never break the session, so every path here exits 0.

BIN="${CLAUDE_PLUGIN_DATA}/bin/forksan"
if [ -x "$BIN" ]; then
    exec "$BIN" hook "$1"
fi

# First run (or broken install): bootstrap in the background, at most once
# per hour, and swallow this event.
STAMP="${CLAUDE_PLUGIN_DATA}/bootstrap-attempt"
mkdir -p "${CLAUDE_PLUGIN_DATA}" 2>/dev/null
if [ ! -f "$STAMP" ] || [ -n "$(find "$STAMP" -mmin +60 2>/dev/null)" ]; then
    touch "$STAMP"
    nohup "${CLAUDE_PLUGIN_ROOT}/scripts/bootstrap.sh" >>"${CLAUDE_PLUGIN_DATA}/bootstrap.log" 2>&1 &
fi
exit 0
