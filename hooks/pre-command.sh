#!/usr/bin/env bash

# Legacy PreToolUse capture cannot provide a current guarded snapshot.
printf '%s\n' 'pre-command retired: configure managed memory-hooks pre-tool' >&2
exit 2
