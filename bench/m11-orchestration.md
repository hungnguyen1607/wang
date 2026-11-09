# M11 multi-agent orchestration

2026-11-08. Code: `crates/wang-daemon/src/orchestrate.rs`.

## What it does

```
task ─► planner (strong model) ─► plan: 2–6 steps, each with a workflow, dependencies, "done when"
         │                         (shown to the user; optional approval with [orchestration] approve_plan)
         ▼
      scheduler: steps whose dependencies succeeded run in parallel (max_parallel, default 2)
         ├─► one worker agent per step
         │     · its own conversation and session: its own jailed browser, its own micro-VM
         │     · its workflow's tools only (research / code / quick / general), its guidance
         │     · gets the results of the steps it depends on, nothing else (small context)
         ├─► checker (cheap model): does the result meet "done when"?
         │     no → the worker retries once with the checker's reason
         │     still no → re-plan the remaining work once; steps behind a failure are skipped
         ▼
      writer (strong model): the final answer from the step results, streamed to the user
```

- **When it runs.** `/plan <task>` always plans, in the terminal, on Telegram, or anywhere else. Without
  the prefix, the free classifier sends a task here when it shows several stages: sequencing words with
  mixed research and code signals, or a numbered list of 3 or more items. Everything else stays in the
  single loop, so simple tasks pay nothing extra.
- **Fallbacks.**
  - A one-step plan runs directly in the single loop, costing only the planner call.
  - An unusable plan falls back to the single loop.
  - A failed checker counts as a pass, so it never blocks the task.
  - A failed writer hands over the raw step results.
- **Budget.** Planned tasks may spend up to 3× the per-task budget. Once that's used, the remaining steps
  are skipped and the reply says why.
- **Metering and tracing.**
  - Planner, checker and writer calls go to the orchestrator's ledger task.
  - Each worker is its own ledger task with source `worker:<task>`, so `wang cost` shows real spend.
  - `wang stats` counts the whole thing as one task.
  - `wang trace <task>` shows the plan, the steps (with worker task ids, attempts and verdicts) and the
    timing.
- **Cleanup.** Each step's browser and VM are released when the step ends.

## Verified (tests and an end-to-end run)

- **Parallelism:** independent steps run at the same time; the test measures 2 workers in flight. The
  dependent step receives both results and its "done when" criterion. Each worker gets its workflow's
  guidance and never plans again.
- **Failure handling:** a step whose result fails the check is retried once with the reviewer's reason,
  then the remaining work is re-planned. The re-planned steps run, steps behind the failure are dropped,
  and the outcome is `partial`. The writer is told which steps failed.
- **Fallbacks:** one-step plans and broken plans fall back to the single loop with no writer call.
- **Plan validation:** the step cap, dropping dependencies on itself, on unknown steps or on later
  steps (so no cycles), renaming duplicate ids, and workers never planning.
- **Real micro-VMs:** two parallel code steps each get their own VM. Each sees only its own files, and
  both 1 s jobs finish together (1.3 s total).
- **Metering:** planner + 3 workers + 3 checks + writer = 8 calls, all in the ledger. Workers don't
  appear as separate tasks in `wang stats`.
- **End to end:** the real daemon and `wang chat`, with a fake OpenRouter server playing each role. The
  plan is printed, s1 and s2 run real sandboxed shell commands in parallel, s3 waits for both, then the
  answer streams. Footer: `plan · 10 calls`. The trace shows s1 and s2 starting at the same moment
  (+0.30 s) and s3 after them.

## Not measured yet (needs a real model)

All runs so far used scripted models, so they prove the mechanics, not the value. The open questions:
- **Success rate** on multi-part tasks, orchestrated vs single loop.
- **Cost overhead:** the planner, checks and writer, against savings from smaller worker contexts and
  cheap-model workers.
- **Wall time** with real model latency, where parallel steps should help most.
- **Classifier precision:** how often auto-planning fires on tasks that didn't need it.

Measuring these needs an eval set of multi-part tasks and an API key. The comparison is `/plan` vs
`/general` on the same tasks.
