# Skills

This directory contains repo-managed skills that can be shared across
clients.

Current layout:

- `review/`: full guide for the cross-agent review workflow
- `review-queue/`: `/review-queue` — check pending reviews
- `review-request/`: `/review-request` — request a review for recent work
- `review-submit/`: `/review-submit` — submit a review verdict

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
