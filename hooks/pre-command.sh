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

exit 0
