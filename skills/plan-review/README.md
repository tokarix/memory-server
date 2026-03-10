# Plan Review Workflow

This skill supports a shared plan review workflow through the memory MCP tools. It is meant for cases where one agent stores a plan for review and another agent later reviews it in a structured, durable way.

## When to use it

Use this workflow when:

- a plan should be stored durably before execution
- review should happen through shared memory instead of ad hoc chat
- you want consistent author and reviewer tags for later search

## Required tools

The workflow relies on memory MCP tools, not raw HTTP:

- `memory_store` for the initial `plan` memory
- `review_queue` to find plans tagged `review-needed`
- `review_submit` to store the review and retag the plan
- `memory_get` to verify the final state

## Plan format

Store the plan as category `plan`.

Required tags:

- `review-needed`
- `author:<agent>`

Recommended tags:

- `session:<external-session-id>`
- `topic:<short-topic>`

The plan content should include:

- goal
- proposed steps
- risks or open questions
- verification or test plan

## Review flow

1. Create or locate the `plan` memory that needs review.
2. Call `review_queue(project="<project>", category="plan")`.
3. Select the relevant queued plan.
4. Review it with a code-review mindset:
   - bugs
   - missing tests
   - risky assumptions
   - behavioral regressions
   - rollout or verification gaps
5. Submit the review with `review_submit(...)`.
6. Verify the plan tags changed from `review-needed` to `reviewed`.

`review_submit` stores the review as a `decision` memory and updates the original plan tags.

## Verdicts

Supported verdicts:

- `approved`
- `changes-requested`
- `rejected`

Use concrete review notes. Prefer findings tied to execution risk over vague impressions.

## Tag conventions

Use stable author and reviewer tags so later searches remain predictable:

- `author:claude`
- `author:codex`
- `reviewed-by:claude`
- `reviewed-by:codex`

After submission, the reviewed plan should typically include:

- `reviewed`
- `reviewed-by:<agent>`
- `review-verdict:<verdict>`

and should no longer include:

- `review-needed`

## Minimal example

Create a temporary test plan:

```text
memory_store(
  project="memory-server",
  category="plan",
  summary="Plan: skill workflow test for shared plan review",
  tags=["review-needed", "author:codex", "topic:plan-review-test", "temporary"],
  content="""
  goal:
  Validate the shared plan-review workflow end to end.

  proposed steps:
  1. Store the plan.
  2. Confirm it appears in the queue.
  3. Review it.
  4. Submit a verdict.
  5. Verify final tags.

  risks or open questions:
  - Tool schema may be stale.

  verification or test plan:
  - Confirm the plan appears in review_queue.
  - Confirm the plan is retagged to reviewed after submission.
  """
)
```

Queue and review it:

```text
review_queue(project="memory-server", category="plan")

review_submit(
  project="memory-server",
  memory_id="<plan-id>",
  reviewer="codex",
  verdict="approved",
  notes="""
  Finding: The plan is narrowly scoped and includes explicit verification.
  Impact: The workflow can be validated with low execution risk.
  Change: None required.
  """
)
```

Verify the final plan state:

```text
memory_get(id="<plan-id>")
```

Expected result:

- the plan no longer appears in `review_queue`
- the plan tags include `reviewed`
- the plan tags no longer include `review-needed`

## Temporary test artifacts

If you are only validating the workflow, mark the plan clearly as a temporary test artifact with tags such as:

- `temporary`
- `topic:plan-review-test`

Include an explicit note in the plan content that it is safe to delete after verification.
