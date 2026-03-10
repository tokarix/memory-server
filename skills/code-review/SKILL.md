---
name: code-review
description: Use when one agent should hand off code for another agent to review, storing review requests as memories tagged `review-needed` and `code-review`.
---

# Code Review

Use this skill when code changes should be handed from one agent to another for structured review through the memory server.

## When to use it

- An agent has completed a feature or fix and wants a second opinion.
- Cross-agent review is needed before merging.
- A project needs a consistent code-review workflow instead of ad hoc prompts.

## Request a code review

Store a `context` memory describing the code to review.

Required tags:

- `code-review`
- `review-needed`
- `author:<agent>`

Recommended tags:

- `session:<external-session-id>`
- `topic:<short-topic>`

The summary should be short and specific.

The content should include:

- git range or commit references (e.g. `abc123..def456`)
- description of the changes
- areas of concern or focus
- how to verify

## Review queued code

1. Call `review_queue(project)` or `review_queue(project, category: "context")` to find items tagged `review-needed`.
2. Select the relevant review request.
3. Inspect the code using `git diff` or `git log` for the range described in the memory content.
4. Review with focus on:
   - correctness and bugs
   - missing tests
   - security concerns
   - API design and naming
   - performance issues
5. Produce a clear verdict:
   - `approved`
   - `changes-requested`
   - `rejected`
6. Call `review_submit(...)` with:
   - `memory_id`
   - `reviewer`
   - `verdict`
   - `notes`

`review_submit` stores the review as a `decision` memory and updates the original from `review-needed` to `reviewed`.

## Review style

- Be concrete.
- Prefer findings over vague impressions.
- Tie criticism to actual risk (bugs, regressions, maintenance burden).
- If the code is acceptable, say why it is acceptable.
- If the code needs changes, explain what is wrong and what should change.

## Polling for reviews

To watch for incoming review requests, instruct the reviewing agent to use `CronCreate` to poll `review_queue` every 5-10 minutes:

```text
CronCreate(
  schedule: "*/5 * * * *",
  command: "review_queue(project=\"<project>\")"
)
```

## Cross-agent convention

Use stable reviewer/author tags so later searches are easy:

- `author:claude`
- `author:codex`
- `reviewed-by:claude`
- `reviewed-by:codex`

If a review supersedes an earlier one, mention that explicitly in the notes.
