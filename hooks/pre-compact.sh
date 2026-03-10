#!/usr/bin/env bash
# PreCompact hook: ingest the session transcript into the memory server
# before Claude Code compacts context.
#
# Install in ~/.claude/settings.json:
# {
#   "hooks": {
#     "PreCompact": [{
#       "hooks": [{
#         "type": "command",
#         "command": "/path/to/memory-server/hooks/pre-compact.sh"
#       }]
#     }]
#   }
# }

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
INGEST="${SCRIPT_DIR}/../target/release/ingest"
# shellcheck source=hooks/_common.sh
source "${SCRIPT_DIR}/_common.sh"

# Read hook input from stdin
input=$(cat)

transcript_path=$(printf '%s' "$input" | jq -r '.transcript_path // empty')
session_id="$(session_id_from_input "$input")"

if [ -z "$transcript_path" ] || [ ! -f "$transcript_path" ]; then
    if [ -n "$session_id" ]; then
        finalize_remote_session "$session_id" || true
    fi
    exit 0
fi

ingest_args=()
config="$(config_path)"
if [ -f "$config" ]; then
    ingest_args+=("$config")
fi
ingest_args+=("$transcript_path")

# Run ingest, but never block compaction
"$INGEST" "${ingest_args[@]}" 2>/dev/null || true

if [ -n "$session_id" ]; then
    finalize_remote_session "$session_id" || true
fi
