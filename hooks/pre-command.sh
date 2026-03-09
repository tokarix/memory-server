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
command="$(command_from_input "$input")"

if [ -z "$session_id" ] || [ -z "$cwd" ] || [ -z "$command" ]; then
    exit 0
fi

mkdir_session_dir "$session_id"
bootstrap_path="$(bootstrap_file "$session_id")"
if [ ! -f "$bootstrap_path" ]; then
    if [ -n "$agent_arg" ]; then
        "${SCRIPT_DIR}/bootstrap.sh" "$agent_arg" <<<"$input" >/dev/null || true
    else
        "${SCRIPT_DIR}/bootstrap.sh" <<<"$input" >/dev/null || true
    fi
fi

project="$(project_from_cwd "$cwd")"
internal_id="$(ensure_remote_session "$session_id" "$cwd" "$project" "$agent" || true)"
metadata="$(metadata_from_input "$input")"
if [ -n "$internal_id" ]; then
    append_remote_message "$session_id" "$internal_id" "$agent" "tool" "command" "$command" "$metadata" || true
fi

if risky_command "$command"; then
    if [ ! -f "$bootstrap_path" ]; then
        echo "Blocked risky command because durable rules were not bootstrapped for this session." >&2
        exit 2
    fi

    echo "Blocked risky command: ${command}" >&2
    echo "Active rule summaries:" >&2
    jq -r '
        (.general_rules + .project_rules)
        | .[]
        | "- " + .summary
    ' "$bootstrap_path" >&2 || true
    exit 2
fi

exit 0
