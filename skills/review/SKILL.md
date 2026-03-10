---
name: review
description: Use when one agent should hand off work (a plan, code changes, or any memory) for another agent to review through the memory server's review tools.
---

# Review

Use this skill when work should be handed from one agent to another for structured review through the memory server. Supports plan reviews, code reviews, and any future review type.

## When to use it

- A plan should be stored durably and reviewed before execution.
- An agent has completed a feature or fix and wants a second opinion.
- Cross-agent review is needed before merging or proceeding.
- A project needs a consistent review workflow instead of ad hoc prompts.

## Core workflow

1. **Author** stores a memory tagged `review-needed` and `author:<agent>`.
2. **Reviewer** calls `review_queue(project)` to find pending items.
3. **Reviewer** evaluates the item and calls `review_submit(...)`.
4. `review_submit` stores a `decision` memory and retags the original from `review-needed` to `reviewed`.

## Request a review

### Plan review

Store the plan as a `plan` memory.

Required tags: `review-needed`, `author:<agent>`

Recommended tags: `session:<external-session-id>`, `topic:<short-topic>`

The content should include:

- goal
- proposed steps
- risks or open questions
- verification or test plan

### Code review

Store a `context` memory describing the code to review.

Required tags: `code-review`, `review-needed`, `author:<agent>`

Recommended tags: `session:<external-session-id>`, `topic:<short-topic>`

The content should include:

- git range or commit references (e.g. `abc123..def456`)
- description of the changes
- areas of concern or focus
- how to verify

## Perform a review

1. Call `review_queue(project)` to see all pending reviews, or filter
   with `review_queue(project, category: "plan")` or
   `review_queue(project, category: "context")`.
2. Select the relevant item.
3. Review it:
   - For plans: bugs, missing tests, risky assumptions, behavioral
     regressions, rollout or verification gaps.
   - For code: correctness, missing tests, security concerns, API
     design, performance issues.
4. Produce a clear verdict:
   - `approved`
   - `changes-requested`
   - `rejected`
5. Call `review_submit(...)` with:
   - `memory_id`
   - `reviewer`
   - `verdict`
   - `notes`

## Review style

- Be concrete.
- Prefer findings over vague impressions.
- Tie criticism to execution risk.
- If the work is acceptable, say why.
- If the work needs changes, explain what is wrong and what should change.

## Suggested note template

- `Finding:` concrete issue or risk
- `Impact:` why it matters
- `Change:` what should be different

## Polling for reviews

To watch for incoming review requests, use `CronCreate` to poll
`review_queue` every 5-10 minutes:

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
