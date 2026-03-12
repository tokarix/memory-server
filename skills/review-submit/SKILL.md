---
name: review-submit
description: Quick command to submit a review verdict for a pending item
---

## Your task

Submit a review. Parse user arguments if provided:

- `/review-submit <id> <verdict> "<notes>"` — submit directly
- `/review-submit <id>` — ask for verdict and notes
- `/review-submit` — call `review_queue` first, let user pick, then ask

Valid verdicts: `approved`, `changes-requested`, `rejected`.

Call `review_submit(memory_id, reviewer: "claude", verdict, notes)`.
Print the stored review ID.

Do not do anything else.
