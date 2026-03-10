#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_CONFIG="${SCRIPT_DIR}/../config.toml"
STATE_ROOT="${TMPDIR:-/tmp}/memory-server-hooks"

config_path() {
    if [ -n "${MEMORY_SERVER_CONFIG:-}" ]; then
        printf '%s\n' "$MEMORY_SERVER_CONFIG"
        return
    fi
    printf '%s\n' "$DEFAULT_CONFIG"
}

config_value() {
    local key="$1"
    local path
    path="$(config_path)"
    if [ ! -f "$path" ]; then
        return 1
    fi

    sed -nE "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"([^\"]*)\"[[:space:]]*$/\1/p" "$path" | head -n1
}

memoryd_url() {
    config_value "memoryd_url" 2>/dev/null || printf 'http://127.0.0.1:8080\n'
}

api_token() {
    config_value "api_token" 2>/dev/null || true
}

auth_args() {
    local token
    token="$(api_token)"
    if [ -n "$token" ]; then
        printf '%s\0%s\0' "-H" "Authorization: Bearer ${token}"
    fi
}

json_field() {
    local input="$1"
    local expr="$2"
    printf '%s' "$input" | jq -r "$expr // empty"
}

session_id_from_input() {
    json_field "$1" '.session_id // .sessionId // .conversation_id // .conversationId // .session.id // .conversation.id'
}

cwd_from_input() {
    json_field "$1" '.cwd // .workspace.cwd // .transcript.cwd // .workspace.root // .root'
}

agent_from_input() {
    local input="$1"
    local explicit="${2:-}"
    if [ -n "$explicit" ]; then
        printf '%s\n' "$explicit"
        return
    fi
    if [ -n "${HOOK_AGENT:-}" ]; then
        printf '%s\n' "$HOOK_AGENT"
        return
    fi
    json_field "$input" '.agent // .client // .client_name // .source // .app // .tool_name'
}

project_from_cwd() {
    local cwd="$1"
    if [ -z "$cwd" ]; then
        printf '\n'
        return
    fi
    basename "$cwd"
}

session_dir() {
    local session_id="$1"
    printf '%s/%s\n' "$STATE_ROOT" "$session_id"
}

bootstrap_file() {
    local session_id="$1"
    printf '%s/bootstrap.json\n' "$(session_dir "$session_id")"
}

state_file() {
    local session_id="$1"
    printf '%s/state.json\n' "$(session_dir "$session_id")"
}

mkdir_session_dir() {
    local session_id="$1"
    mkdir -p "$(session_dir "$session_id")"
}

fetch_bootstrap() {
    local project="$1"
    local out_file="$2"
    local url
    url="$(memoryd_url)/api/v1/projects/${project}/bootstrap?include_general=true&include_recall=true"

    local curl_args=()
    while IFS= read -r -d '' arg; do
        curl_args+=("$arg")
    done < <(auth_args)

    curl -fsS "${curl_args[@]}" "$url" >"$out_file"
}

extract_text() {
    local input="$1"
    printf '%s' "$input" | jq -r '
        def textify:
            if type == "string" then .
            elif type == "array" then
                map(select(.type == "text") | .text // empty)
                | map(select(length > 0))
                | join("\n")
            elif type == "object" then
                (.text // .content // "")
            else
                ""
            end;

        (.last_assistant_message // .message.content // .content // .prompt // .response // .text // .message.text // "")
        | textify
    '
}

command_from_input() {
    local input="$1"
    json_field "$input" '.command // .tool_input.command // .tool_input.cmd // .input.command // .toolInput.command // .toolInput.cmd'
}

metadata_from_input() {
    local input="$1"
    printf '%s' "$input" | jq -c 'del(.message.content?, .content?, .prompt?, .response?, .text?)'
}

ensure_state_file() {
    local session_id="$1"
    local file
    file="$(state_file "$session_id")"
    if [ ! -f "$file" ]; then
        jq -cn --arg external_session_id "$session_id" \
            '{external_session_id:$external_session_id}' >"$file"
    fi
}

state_value() {
    local session_id="$1"
    local expr="$2"
    local file
    file="$(state_file "$session_id")"
    if [ ! -f "$file" ]; then
        return 1
    fi
    jq -r "$expr // empty" "$file"
}

update_state_file() {
    local session_id="$1"
    shift
    local tmp
    tmp="$(mktemp)"
    jq "$@" "$(state_file "$session_id")" >"$tmp"
    mv "$tmp" "$(state_file "$session_id")"
}

ensure_remote_session() {
    local session_id="$1"
    local cwd="$2"
    local project="$3"
    local agent="$4"

    mkdir_session_dir "$session_id"
    ensure_state_file "$session_id"

    local internal_id
    internal_id="$(state_value "$session_id" '.internal_session_id')" || true
    if [ -n "$internal_id" ]; then
        printf '%s\n' "$internal_id"
        return
    fi

    local payload url
    payload="$(jq -cn \
        --arg agent "$agent" \
        --arg cwd "$cwd" \
        --arg external_session_id "$session_id" \
        --arg project "$project" \
        '{agent:($agent|if length>0 then . else null end),cwd:$cwd,external_session_id:$external_session_id,project:$project}')"
    url="$(memoryd_url)/api/v1/sessions/start"

    local curl_args=()
    while IFS= read -r -d '' arg; do
        curl_args+=("$arg")
    done < <(auth_args)

    local response http_code
    response="$(printf '%s' "$payload" | curl -sS "${curl_args[@]}" \
        -H "Content-Type: application/json" \
        -X POST \
        --data-binary @- \
        -w '\n%{http_code}' \
        "$url")"
    http_code="$(printf '%s' "$response" | tail -n1)"
    response="$(printf '%s' "$response" | sed '$d')"
    if [ "$http_code" != "200" ] && [ "$http_code" != "201" ]; then
        echo "ensure_remote_session: HTTP ${http_code} from ${url}" >&2
    fi
    internal_id="$(printf '%s' "$response" | jq -r '.session.id // empty')"
    if [ -n "$internal_id" ]; then
        update_state_file "$session_id" \
            --arg internal_session_id "$internal_id" \
            --arg project "$project" \
            --arg cwd "$cwd" \
            --arg agent "$agent" \
            '.internal_session_id=$internal_session_id | .project=$project | .cwd=$cwd | .agent=$agent'
        printf '%s\n' "$internal_id"
    fi
}

append_remote_message() {
    local session_id="$1"
    local internal_id="$2"
    local agent="$3"
    local role="$4"
    local kind="$5"
    local content="$6"
    local metadata="$7"

    local payload url
    payload="$(printf '%s' "$content" | jq -Rsc \
        --arg agent "$agent" \
        --arg kind "$kind" \
        --arg metadata "$metadata" \
        --arg role "$role" \
        '{agent:($agent|if length>0 then . else null end),content:.,kind:$kind,metadata:($metadata|if length>0 then . else null end),role:$role}')"
    url="$(memoryd_url)/api/v1/sessions/${internal_id}/messages"

    local curl_args=()
    while IFS= read -r -d '' arg; do
        curl_args+=("$arg")
    done < <(auth_args)

    local http_code
    http_code="$(printf '%s' "$payload" | curl -sS "${curl_args[@]}" \
        -H "Content-Type: application/json" \
        -X POST \
        --data-binary @- \
        -o /dev/null \
        -w '%{http_code}' \
        "$url")"
    if [ "$http_code" != "200" ] && [ "$http_code" != "201" ]; then
        echo "append_remote_message: HTTP ${http_code} from ${url}" >&2
    fi
}

finalize_remote_session() {
    local session_id="$1"
    local internal_id
    internal_id="$(state_value "$session_id" '.internal_session_id')" || return 0
    if [ -z "$internal_id" ]; then
        return 0
    fi

    local payload url
    payload='{}'
    url="$(memoryd_url)/api/v1/sessions/${internal_id}/finalize"

    local curl_args=()
    while IFS= read -r -d '' arg; do
        curl_args+=("$arg")
    done < <(auth_args)

    local http_code
    http_code="$(printf '%s' "$payload" | curl -sS "${curl_args[@]}" \
        -H "Content-Type: application/json" \
        -X POST \
        --data-binary @- \
        -o /dev/null \
        -w '%{http_code}' \
        "$url")"
    if [ "$http_code" != "200" ] && [ "$http_code" != "201" ]; then
        echo "finalize_remote_session: HTTP ${http_code} from ${url}" >&2
    fi
}

summarize_bootstrap() {
    local file="$1"
    jq -r '
        [
          ("Project: " + .project),
          ("General rules: " + ((.general_rules | length) | tostring)),
          ("Project rules: " + ((.project_rules | length) | tostring)),
          ("Recall memories: " + ((.recall_memories | length) | tostring))
        ] | join("\n")
    ' "$file"
}

risky_command() {
    local command="$1"
    case "$command" in
        *"rm -rf "*|*" git reset --hard"*|*"git reset --hard"*|*" git checkout -- "*|*"git checkout -- "*|*"git clean -fd"*|*"git clean -xdf"*|*"mkfs "*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}
