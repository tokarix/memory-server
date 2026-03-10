#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SKILLS=(code-review plan-review)

usage() {
    cat <<'EOF'
Usage: install-skills.sh [codex|claude|all]

Symlinks repo-managed skills into the local client skill directories.

Targets:
  codex   Install into ~/.codex/skills
  claude  Install into ~/.claude/skills
  all     Install into both (default)
EOF
}

install_one() {
    local target_root="$1"
    mkdir -p "$target_root"
    for skill in "${SKILLS[@]}"; do
        local source="${REPO_ROOT}/skills/${skill}"
        local target="${target_root}/${skill}"
        rm -f "$target"
        ln -s "$source" "$target"
        printf 'Installed %s -> %s\n' "$target" "$source"
    done
}

target="${1:-all}"
case "$target" in
    codex)
        install_one "/var/home/stintel/.codex/skills"
        ;;
    claude)
        install_one "/var/home/stintel/.claude/skills"
        ;;
    all)
        install_one "/var/home/stintel/.codex/skills"
        install_one "/var/home/stintel/.claude/skills"
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        usage >&2
        exit 1
        ;;
esac
