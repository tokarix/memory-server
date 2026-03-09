#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=hooks/_common.sh
source "${SCRIPT_DIR}/_common.sh"

input="$(cat)"
session_id="$(session_id_from_input "$input")"

if [ -z "$session_id" ]; then
    exit 0
fi

finalize_remote_session "$session_id" || true
