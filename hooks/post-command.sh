#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=hooks/_common.sh
source "${SCRIPT_DIR}/_common.sh"

agent_arg="${1:-}"
input="$(cat)"

session_id="$(session_id_from_input "$input")"
cwd="$(cwd_from_input "$input")"
agent="$(agent_from_input "$input" "$agent_arg")"

if [ -z "$session_id" ] || [ -z "$cwd" ]; then
    exit 0
fi

# Extract tool output from PostToolUse input
output="$(json_field "$input" '.tool_response.stdout // .tool_response.content // .tool_result // .tool_output // .output // .result // ""')"
if [ -z "$output" ]; then
    exit 0
fi

project="$(project_from_cwd "$cwd")"
internal_id="$(ensure_remote_session "$session_id" "$cwd" "$project" "$agent" || true)"
metadata="$(metadata_from_input "$input")"
if [ -n "$internal_id" ]; then
    append_remote_message "$session_id" "$internal_id" "$agent" "tool" "output" "$output" "$metadata" || true
fi
