#!/usr/bin/env bash

set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "Usage: capture-message.sh [agent] <user|assistant>" >&2
    exit 1
fi

if [ "$#" -eq 1 ]; then
    agent_arg=""
    role="$1"
else
    agent_arg="$1"
    role="$2"
fi
if [ "$role" != "user" ] && [ "$role" != "assistant" ]; then
    echo "capture-message.sh role must be user or assistant" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=hooks/_common.sh
source "${SCRIPT_DIR}/_common.sh"

input="$(cat)"
session_id="$(session_id_from_input "$input")"
cwd="$(cwd_from_input "$input")"
agent="$(agent_from_input "$input" "$agent_arg")"
text="$(extract_text "$input")"

if [ -z "$session_id" ] || [ -z "$cwd" ] || [ -z "$text" ]; then
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
if [ -z "$internal_id" ]; then
    exit 0
fi

append_remote_message "$session_id" "$internal_id" "$agent" "$role" "message" "$text" "" || true
