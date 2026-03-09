# Skills

This directory contains repo-managed skills that can be shared across
clients.

Current layout:

- `plan-review/`: shared workflow for storing plans tagged
  `review-needed` and reviewing them through the memory server's plan
  review tools

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
