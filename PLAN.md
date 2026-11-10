# wang: a secure, lightweight, token-cheap agent harness + infra for your laptop

**What it is:** your own OpenClaw-style assistant, built as agent infrastructure: a Rust harness (loop, tools, context,
memory, routing, evals) on top of infra (sandbox, browser runtime, micro-VMs, token proxy).
- **Secure:** every tool runs in a sandbox; big jobs run in a throwaway micro-VM.
- **Light:** a Rust daemon, because the laptop already runs at 7.6/8 GB of swap.
- **Cheap:** token optimization and cost tracking from the first commit.
- **Measurable:** every step is traced, and the headline numbers show agent quality, infra and scale.

**Setup:**
- Rust ~85%, optional C++ ~15%, in one Cargo workspace. Each piece also ships as its own tool.
- Folder: `/home/hung/wang`. Models through OpenRouter, with the key in `.env`.
- Order: core infrastructure first, phone access (Telegram) after.

## Architecture

```
`wang chat` (terminal, M1)  /  Telegram bot (M5.5)        ntfy / Telegram (push + approvals)
                 ▼
wang-daemon (Rust) ── agent loop ── model calls ─► wang-tokens (optimizer + ledger) ─► OpenRouter
     │   ├─ memory (Markdown + tantivy search)
     │   └─ every step traced ─► wang-trace (SQLite)
     ▼ tools run inside
wang-jail (namespaces, cgroups, seccomp, domain allow-list)
     ├─ wang-browser (Chrome over CDP, compact page view, pool)
     └─ wang-vm (Firecracker desktop: snapshot / fork / rollback)
wang-evals (recorded tasks, exact replay)   wang-bench (fake model server, 1→1000 agents)
```

## Milestones

### Core infrastructure

**M1: Agent core + cost ledger + traces**
- Runs as a systemd service. You talk to it with `wang chat` in the terminal, with replies streamed live.
- Agent loop with 3 tools: read file, shell, web fetch.
- Model client that works with any provider (OpenRouter by default).
- Prompts ordered so prompt caching hits; every step traced.
- *Done when:* a multi-step task completes, and `wang cost today` and `wang trace <id>` work.

**M2: Agent health**
- `wang stats`: success rate, cost per successful task, which tools fail most.
- Loop detection: the same call failing N times aborts the task and sends you an alert through ntfy.

**M3: Sandbox (`wang-jail`)**
- Each tool declares the files, domains and CPU/RAM limits it needs. The sandbox enforces exactly that with Linux
  namespaces, an overlay filesystem, cgroups, seccomp, and an egress proxy that only allows the declared domains.
- Every tool runs through it, and huge tool outputs get trimmed.
- Ships on its own as the `jail` CLI.
- *Done when:* every escape test is blocked (reading `~/.ssh`, unlisted domains, fork bomb, memory bomb), and the
  overhead per call is measured.

**M4: Memory, approvals, budgets**
- Memory: Markdown fact files plus search.
- Risky actions need your approve/deny through ntfy.
- Old conversation history gets summarized when it grows too long.
- Each task has a token/$ budget; when it runs out, the agent pauses and asks you.

**M5: Evals + scale bench + model routing**
- Evals: 30 real tasks of yours, recorded and replayed, measuring success, cost, steps and time.
- Scale bench (`wang bench`): a fake model server replays recorded responses, so it costs $0. It ramps from 1 to 1000
  agents running at once, each doing real tool calls in its own sandbox. Find the first bottleneck and fix it.
- Routing: a cheap model for simple steps, kept only if success holds.

### Then
- **M5.5: Telegram.** (Replaced the PWA on 2026-11-07: less to build and host.) A bot the daemon long-polls (no
  open port): chat with streamed replies, Approve/Deny buttons, `/cost` `/stats` `/trace` `/good` `/bad` `/new`,
  alerts. Allow-listed user ids, private chats only. Trade-off: bot chats aren't end-to-end encrypted.
- **M6: Standalone token proxy**, so Claude Code and opencode users get the same savings. (Built 2026-11-07:
  `wang proxy` / `wang-proxy` on loopback for the Anthropic and OpenAI-compatible APIs. It trims huge tool outputs,
  replaces repeated ones, adds a cache hint when the client sent none, and records usage in the ledger. It supports
  a daily budget, and its rewrites keep the history append-only. Checked with real Claude Code against a fake provider.)
- **M7: Browser runtime.** Your own Chrome control client, compact page view, a pool of browsers with RAM caps, all
  inside the sandbox. (Built 2026-11-08: CDP over `--remote-debugging-pipe`, one jailed browser per conversation with
  a RAM cap each, and a `browser` tool that returns compact text with numbered elements. The view is 1.5–50× smaller
  than the HTML on real pages. Also fixed two sandbox bugs: commands killed after 10 s, and a shared run directory.
  Results in bench/m7-browser.md.)
- **M8: VM desktop.** Firecracker: boots in under 1 s; snapshot, fork into N copies, rollback; memory sharing between
  VMs (KSM); try N approaches in parallel and keep the best. (Built 2026-11-08 as a command-line Linux VM, not a
  graphical desktop: image from any Docker image, Firecracker inside wang's jail, cold boot 0.3 s, restore 77 ms,
  8 forks in about 0.3 s at 5 MiB each idle, and a `vm` tool with checkpoint/rollback/try. KSM is enrolled, but the
  kernel's scan rate is a root setting. Also fixed the jail's SIGPIPE race, which killed about 2% of jails at once
  under load. Results in bench/m8-vm.md.)
- **M9 (optional C++):** a small local model for routine decisions and/or a fast screen-diff module. (Scoped
  2026-11-08: the local model only, llama.cpp `llama-server` with a ~1 GB model, used first for history summaries
  at $0. Screen-diff waits until something produces screenshots. Built: jailed llama-server on a Unix socket that
  starts on demand. Measured 20–40 s per summary on this laptop and it invented/mislabeled facts, to save ~$0.0002,
  so it ships off by default. Results in bench/m9-local.md.)
- **M10: MCP server + task-shaped workflows.** `wang mcp`: wang's sandboxed shell, browser and micro-VMs as MCP tools
  for Claude Code / Claude Desktop (`claude mcp add wang -- wang mcp`). Users bring their own Claude, so no API keys
  and $0 for us. Plus agent workflows designed per kind of task (e.g. research → browser, risky code → VM with
  try-N, quick edits → shell) instead of one generic loop. (Built 2026-11-08: `wang mcp` with tools and prompts,
  checked end to end with real Claude Code 2.1.318 against a fake model ($0): ToolSearch loaded wang's tools, then a VM
  run, a sandboxed shell with ~/.ssh hidden, and vm try. Workflows research/code/quick/general use a keyword
  classifier plus `/workflow` overrides; guidance goes with the message and tools are gated, with the tool list kept
  fixed for caching. Also in Telegram and `wang chat`.)
- **M11: Multi-agent orchestration.** Think before doing: a planner writes the pipeline, worker agents run the steps
  (in parallel when independent, each in its own sandbox, browser and VM), a checker verifies each step with retry and
  re-plan, and a writer composes the answer. (Built 2026-11-08: `/plan` or automatic for tasks with several stages,
  optional plan approval, budgets, full traces. Verified with scripted models and real micro-VMs; success and cost
  against the single loop still need live evals. Results in bench/m11-orchestration.md.)
- **M12: Daily assistant + one-command install.** (Built 2026-11-08.)
  - Scheduled tasks: plain-English times, kept in SQLite, run by the daemon through the full agent, results to the
    Telegram chat or the alerts, plus a `schedule` tool for the agent.
  - wio-style onboarding: `install.sh` (checksum-verified, ~/.local, no sudo), then bare `wang` runs setup the first
    time (doctor, optional Claude Code hookup, optional key, optional service; it asks before each). After that,
    `wang` starts the daemon and opens chat.
  - Release packaging and CI workflows (written; they run once the repo is on GitHub).
  - A landing page.
- **Next (not started):** integrations (email, calendar), more chat apps, skills; the live-model run and evals; a
  security pass; a rename (done 2026-11-08: warp → wang, since "Warp" was taken).

## Token optimization

Measure first, optimize second, and keep a change only if the evals hold:
- M1: cost ledger and cache-friendly prompts
- M3: trimming and deduplicating tool output
- M4: history summarizing and budgets
- M5: model routing
- M7: compact page view
- M9: local model

## Recorded on every step (from M1)

- **Task:** goal, outcome, your 👍/👎.
- **Model call:** model, tokens (input / cached / output / reasoning), cost, time to first token, latency, errors, prompt size.
- **Tool call:** output size, whether it was trimmed, latency, errors, whether it repeated an earlier call.
- **Sandbox:** startup overhead, RAM, CPU, what it blocked.
- **Approval:** your decision and how long you took.

## Headline metrics

- **Agent:** success rate (vs a plain agent loop without your harness); cost per successful task before → after;
  % of tasks done with no human help; steps per task; failed tool calls.
- **Infra:** sandbox overhead per call and 100% of escapes blocked; daemon idle RAM/CPU; VM boot and fork time.
- **Scale:** max agents at once on the laptop; RAM per agent; steps per second; the success gain from trying N
  approaches in parallel.

## How we check it works

- Unit tests for each part, plus the escape tests on every change.
- Evals after every change, compared with the previous run; anything that makes it worse gets rejected.
- Bench results saved at each milestone.
- `wang cost` matches the OpenRouter dashboard within a few %.
