---
name: review-request
description: Quick command to request a review for recent work by storing a review-needed memory
---

## Your task

Store a memory tagged `review-needed` so another agent can review it.

1. Gather git context by running these commands via Bash in the project directory:
   - `git branch --show-current`
   - `git log --oneline -10`
   - `git diff HEAD --stat`
2. Determine review type from user input or ask briefly: plan or code?
3. Call `memory_store`:
   - **Plan**: category `plan`, tags `["review-needed", "author:claude"]`
   - **Code**: category `context`, tags `["review-needed", "code-review", "author:claude"]`
4. Auto-fill git range and file list from the git context gathered in step 1.
5. Keep summary to one line. Include enough content for a reviewer to act.
6. Print the memory ID.

Do not do anything else.
