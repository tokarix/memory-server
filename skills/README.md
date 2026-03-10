# Skills

This directory contains repo-managed skills that can be shared across
clients.

Current layout:

- `review/`: shared workflow for requesting and performing reviews
  (plans, code, etc.) through the memory server's review tools

Install local symlinks with:

```sh
./scripts/install-skills.sh all
```

Targets:

- `codex`: symlink into `~/.codex/skills/`
- `claude`: symlink into `~/.claude/skills/`
- `all`: install both

The repo copy is the source of truth. The client skill directories should
contain symlinks back to this tree during development.
