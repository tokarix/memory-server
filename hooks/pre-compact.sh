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
CONFIG="${SCRIPT_DIR}/../config.toml"

# Read hook input from stdin
input=$(cat)

transcript_path=$(printf '%s' "$input" | jq -r '.transcript_path // empty')

if [ -z "$transcript_path" ] || [ ! -f "$transcript_path" ]; then
    exit 0
fi

ingest_args=()
if [ -f "$CONFIG" ]; then
    ingest_args+=("$CONFIG")
fi
ingest_args+=("$transcript_path")

# Run ingest, but never block compaction
"$INGEST" "${ingest_args[@]}" 2>/dev/null || true
