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

project="$(project_from_cwd "$cwd")"
if [ -z "$project" ]; then
    exit 0
fi

mkdir_session_dir "$session_id"
bootstrap_path="$(bootstrap_file "$session_id")"
tmp_file="${bootstrap_path}.tmp"
internal_id="$(ensure_remote_session "$session_id" "$cwd" "$project" "$agent" || true)"

if fetch_bootstrap "$project" "$tmp_file"; then
    mv "$tmp_file" "$bootstrap_path"
    if [ -n "$internal_id" ]; then
        update_state_file "$session_id" \
            --arg project "$project" \
            --arg cwd "$cwd" \
            --arg agent "$agent" \
            '.project=$project | .cwd=$cwd | .agent=$agent'
    fi
    summarize_bootstrap "$bootstrap_path"
else
    rm -f "$tmp_file"
    echo "Bootstrap fetch failed for project ${project}" >&2
fi
