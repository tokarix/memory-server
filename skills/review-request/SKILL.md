---
name: review-request
description: Quick command to request a review for recent work by storing a review-needed memory
---

## Context

- Current branch: !`git branch --show-current`
- Recent commits: !`git log --oneline -10`
- Changed files: !`git diff HEAD --stat`

## Your task

Store a memory tagged `review-needed` so another agent can review it.

1. Determine review type from user input or ask briefly: plan or code?
2. Call `memory_store`:
   - **Plan**: category `plan`, tags `["review-needed", "author:claude"]`
   - **Code**: category `context`, tags `["review-needed", "code-review", "author:claude"]`
3. Auto-fill git range and file list from the context above.
4. Keep summary to one line. Include enough content for a reviewer to act.
5. Print the memory ID.

Do not do anything else.
