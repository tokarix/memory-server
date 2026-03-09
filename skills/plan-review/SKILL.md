---
name: plan-review
description: Use when one agent should hand off a plan for another agent to review, especially when storing `plan` memories tagged `review-needed` and submitting a review with the memory server's plan review tools.
---

# Plan Review

Use this skill when a plan should be handed from one agent to another for structured review through the memory server.

## When to use it

- A plan should be stored durably and reviewed later.
- Claude creates a plan and Codex reviews it.
- Codex creates a plan and Claude reviews it.
- A project needs a consistent review workflow instead of ad hoc prompts.

## Create a plan for review

Store the plan as a `plan` memory.

Required tags:

- `review-needed`
- `author:<agent>`

Recommended tags:

- `session:<external-session-id>`
- `topic:<short-topic>`

The summary should be short and specific.

The content should include:

- goal
- proposed steps
- risks or open questions
- verification or test plan

## Review a queued plan

1. Call `plan_review_queue(project)` to find plans tagged `review-needed`.
2. Select the relevant plan.
3. Review it with a code-review mindset:
   - bugs
   - missing tests
   - risky assumptions
   - behavioral regressions
   - rollout or verification gaps
4. Produce a clear verdict:
   - `approved`
   - `changes-requested`
   - `rejected`
5. Call `plan_review_submit(...)` with:
   - `plan_id`
   - `reviewer`
   - `verdict`
   - `notes`

`plan_review_submit` stores the review as a `decision` memory and updates the original plan from `review-needed` to `reviewed`.

## Review style

- Be concrete.
- Prefer findings over vague impressions.
- Tie criticism to execution risk.
- If the plan is acceptable, say why it is acceptable.
- If the plan is weak, explain what is missing and what should change.

## Suggested templates

Plan summary:

`Plan: <project task in one line>`

Review notes:

- `Finding:` concrete issue or risk
- `Impact:` why it matters
- `Change:` what should be different

## Cross-agent convention

Use stable reviewer/author tags so later searches are easy:

- `author:claude`
- `author:codex`
- `reviewed-by:claude`
- `reviewed-by:codex`

If a review supersedes an earlier one, mention that explicitly in the notes.
