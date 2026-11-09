# wang

A secure, lightweight, token-cheap agent harness for your laptop. See [PLAN.md](PLAN.md) for the roadmap.

## Status: M1 (agent core + ledger + traces), M2 (agent health), M3 (sandbox), M4 (memory, approvals, budgets), M5 (evals, scale bench, routing), M5.5 (Telegram), M6 (token proxy), M7 (browser), M8 (micro-VMs), M9 (local model, off by default), M10 (MCP for Claude + task workflows), M11 (multi-agent orchestration), M12 (scheduled tasks + one-command install)

- `wang run` daemon (Rust, tokio, ~10 MB RSS), runnable as a systemd user service
- `wang chat` talks to it over a Unix socket (`$XDG_RUNTIME_DIR/wang.sock`, chmod 600, peer-uid checked) with streamed replies
- Agent loop with `read_file`, `shell`, `web_fetch`; parallel tool calls run concurrently
- Model clients: **OpenRouter** (default, streaming SSE, `anthropic/claude-haiku-5.5`) and direct Claude API
- Cache-friendly prompts: fixed tools → static system prompt → append-only history; Claude models get explicit + automatic cache breakpoints, and the session id pins the OpenRouter provider so the cache stays warm
- Cost ledger: provider-reported cost per call (OpenRouter `usage.cost`), uncached / cache-write / cache-read / output / reasoning tokens
- Traces (`wang-trace`): a span for every task, step, model call and tool call, with OpenTelemetry GenAI attribute names: tokens, cost, time to first token, latency, request size, tool output size, trimmed, repeated, errors, 👍/👎
- Tool output caps (head + tail)
- `wang stats`: success rate (finished and not rated 👎), cost per successful task (all spend / successes), outcomes, cache hit, steps per task, model latency and first-token time, per-tool failure rates, and the exact calls that fail most
- Loop detection: the same call (tool + input) failing 3× or being made 5× aborts the task with an explanation, and sends an ntfy alert if `[ntfy] topic` is set

- **Sandbox (`wang-jail`)**: `shell` and `read_file` run in a jail: new user/mount/PID/net/IPC/UTS namespaces (bubblewrap), only declared paths visible, secrets always hidden (`~/.ssh`, `~/.gnupg`, cloud/CLI credentials, browser profiles, shell history, wang's own config and DB), cgroup caps on RAM (no swap), CPU and processes, a seccomp deny-list, and no network except a domain allow-list enforced by an egress proxy that also refuses localhost/LAN/tailnet addresses. `web_fetch` gets the same domain allow-list and SSRF blocking in-process. Fails closed if the sandbox can't start. 16/16 escape tests blocked; ~45 ms overhead per call ([bench/m3-jail.md](bench/m3-jail.md))
- **Memory**: facts in `~/.local/share/wang/memory.md` (plain Markdown, edit freely); `remember` / `recall` tools; relevant facts (BM25 search) are attached to your message, not the system prompt, so the prompt cache isn't broken; `wang memory` lists or adds facts
- **Approvals**: `shell` runs read-only with no network by default; a command that needs your `write` paths or network allow-list must ask with `privileged: true`, and waits for your yes/no: Approve/Deny buttons on the ntfy notification (posted to `<topic>-reply`, which the daemon listens on), `y/N` in `wang chat`, or `wang approve|deny <id>`. Unanswered = denied after 10 minutes. With the sandbox off, every shell command asks
- **Budgets**: $0.25 per task by default; when it's used up, wang pauses, tells you, and alerts via ntfy; "continue" gives it another budget
- **History summarizing**: past 60k prompt tokens, older turns are replaced by a model-written summary (the last 2 user turns stay word-for-word)
- **Evals** (`wang-evals`): tasks in `evals/tasks/*.toml` (prompt, fixture files, checks: reply contains / shell check in the fixture folder). `wang eval --live` runs the real model and records cassettes; `wang eval` replays them for $0, deterministically, and compares with the previous run (exit 1 on any regression). 8 starter tasks included; replace them with your own
- **Routing**: `[routing]` sends step 1 to the main model and later steps to a cheap one; off until `wang eval --live --routing` shows success holds
- **Scale bench** (`wang-bench`): `wang bench` ramps 1 → 1000 real agents at once against a fake OpenRouter-compatible model server ($0), each doing real sandboxed tool calls, and reports tasks/s, steps/s, task / model / tool latency, event-loop stalls and memory per agent; stops early on errors or low memory. 1000 agents, 0 failures, ~0.15 MB RSS per agent; bottlenecks found and fixed (per-commit fsync: 10×; jail setup on the event loop) and the remaining ceiling (the kernel creates ~75 sandboxes/s) in [bench/m5-scale.md](bench/m5-scale.md)
- **Telegram** (`telegram.rs`): chat with wang from your phone. Replies stream by editing the message in place; Approve/Deny buttons for privileged actions (from any session, `wang chat` included); loop/budget alerts; `/new` `/cost` `/stats` `/trace` `/good` `/bad`. Long polling, so no open port. Only `allowed_user_ids`, only private chats; strangers get no reply and their id is logged once. The token stays out of logs. Note: bot chats aren't end-to-end encrypted, so Telegram sees prompts and replies
- **Browser** (`wang-browser`): a `browser` tool runs headless Chrome inside the jail, one browser per conversation. Each browser has its own RAM cap and the pool limits how many run; idle ones close, and none start when memory is low. wang controls Chrome with its own DevTools client over a pipe, so no port is opened. Pages load only from the browser's domain allow-list, through the jail's egress proxy. The agent never sees HTML: pages come back as compact text, with every link, button and field numbered as `[n ...]`, and the agent clicks, types and selects by number. On real pages the view is 1.5–50× smaller than the HTML, about the size of the plain text. Results are in [bench/m7-browser.md](bench/m7-browser.md). `wang browse URL` shows what the agent sees
- **Micro-VMs** (`wang-vm`): a `vm` tool runs big or risky jobs in Firecracker micro-VMs, and Firecracker itself runs inside wang's jail. The VM image is built from any Docker image. Its root is read-only with a RAM overlay, it has no network device, and it reaches only allowed domains through wang's egress proxy. A cold boot takes 0.3 s; new VMs restore a base snapshot in about 80 ms. 8 forks start in about 0.3 s and cost 5 MiB each while idle. The agent can `checkpoint` and `rollback`, or `try` several commands in parallel copies of the VM, score each, and keep the best copy. Results are in [bench/m8-vm.md](bench/m8-vm.md)
- **Local model** (`model/local.rs`): llama.cpp running Qwen2.5-1.5B inside the jail, with no network, reached over a private Unix socket. It starts on first use and stops when idle, and it can write history summaries for $0. It's off by default: on this laptop it takes 20–40 s per summary and sometimes invents or mislabels facts, all to save about $0.0002. See [bench/m9-local.md](bench/m9-local.md). Turn it on with `wang local setup` and `[local] enabled = true`
- **Multi-agent orchestration** (`orchestrate.rs`): a multi-part task is planned first, then executed by separate agents:
  - a planner splits the task into steps, each with a workflow, dependencies and a "done when" check;
  - one worker agent per step, each with its own conversation, browser and micro-VM, running in parallel when their dependencies allow;
  - a checker verifies each result; a failed step is retried with feedback, then the rest is re-planned;
  - a writer streams the answer from the step results.

  Use `/plan <task>`, or let the classifier pick it for tasks with several stages. Everything is metered and traced. See [bench/m11-orchestration.md](bench/m11-orchestration.md)
- **Scheduled tasks** (`schedule.rs`): wang works on a timer.
  - Ask it "every weekday at 8am, send me the top 5 Hacker News posts", or run `wang schedule add "every 2 hours" …`. Times are plain English: `every monday at 18:30`, `in 20 minutes`, `tomorrow at 9am`.
  - Each run goes through the full agent: workflows, plans, sandbox and budgets.
  - Results come back to the Telegram chat the schedule was made in, or to your alerts.
  - Schedules are kept in SQLite. Runs missed while the daemon was off are caught up if within 12 hours, otherwise skipped.
  - On Telegram: `/schedules`, `/unschedule`.
- **One-command install** (`scripts/install.sh`, `wang setup`, `wang doctor`): `curl … | sh` downloads the release for your machine, checks its SHA-256, and installs it to `~/.local` with no sudo. Then bare `wang` sets itself up the first time:
  - checks the sandbox, browser and VMs, with the fix for anything missing;
  - offers to plug into Claude Code;
  - takes an API key if you want wang's own agent;
  - offers a background service.

  It asks before each change. After that, `wang` starts the daemon if needed and opens chat
- **Use wang from Claude** (`wang mcp`): wang's tools become an MCP server for Claude Code and Claude Desktop. Claude does the thinking on your own subscription, so there's no API key and no extra cost. wang provides the sandboxed shell and file reader, `web_fetch` behind the allow-list, the jailed browser, throwaway micro-VMs with checkpoint/rollback/try, memory, and the cost ledger. Shell commands that write or use the network are refused by default, and the tool points Claude to the VM instead. The workflows are offered as MCP prompts (`/mcp__wang__research`, `/mcp__wang__code`, `/mcp__wang__quick`). Tested with real Claude Code 2.1.318
- **Task workflows** (`workflow.rs`): each message is sorted into *research* (web, several sources, citations), *code* (in a micro-VM, checkpoint, try alternatives and keep what passes), *quick* (local tools, 6 steps max) or *general* (everything). The sorting uses a free keyword scorer, or you can start the message with `/research`, `/code`, `/quick` or `/general`. A workflow adds its guidance to the message, sets the step limit, and refuses tools outside it, naming the right ones. The tool list never changes, so the prompt cache stays warm. The footer shows which workflow ran, and so does the trace
- **Token proxy** (`wang-proxy`): `wang proxy` (or the standalone `wang-proxy`) gives Claude Code, opencode and other agents wang's savings and cost tracking. It runs on loopback and supports the Anthropic Messages API and OpenAI-style chat completions. It trims huge tool outputs and replaces repeated ones with a pointer to the earlier call. When a request has no `cache_control`, it adds a cache hint. Responses stream through unchanged while usage is recorded in wang's ledger, so `wang cost` includes these agents and `wang proxy stats` shows what was saved. Each message is rewritten the same way on every request, so the history the provider sees only grows at the end. That keeps the prompt cache and thinking signatures valid. It stores no keys and forwards the client's own auth headers. `--daily-budget` refuses calls once the day's spend reaches the limit
- Identical tool output from an identical earlier call is deduplicated (the model gets a pointer, not the same tokens again)

## Setup

```bash
cargo build --release
./target/release/wang init                 # ~/.config/wang/config.toml and ~/.config/wang/.env (chmod 600)
$EDITOR ~/.config/wang/.env                # OPENROUTER_API_KEY=sk-or-...
./target/release/wang run                  # foreground daemon (Ctrl-C to stop)
./target/release/wang chat                 # in another terminal
```

As a service:

```bash
./target/release/wang service install      # writes ~/.config/systemd/user/wang.service
systemctl --user daemon-reload && systemctl --user enable --now wang
journalctl --user -u wang -f
```

## Telegram

1. Create a bot with [@BotFather](https://t.me/BotFather) and put the token in `~/.config/wang/.env`:
   `TELEGRAM_BOT_TOKEN=123456:ABC...`
2. Start the daemon and message your bot once. It ignores you, but logs your id:
   `telegram: ignored a message from user id 123456789 (@you)`
3. Add it to `~/.config/wang/config.toml` and restart:

```toml
[telegram]
allowed_user_ids = [123456789]
stream = true      # edit the reply live
alerts = true      # alerts + Approve/Deny buttons for every session
```

## Token proxy

```
wang proxy                                   # 127.0.0.1:8788
ANTHROPIC_BASE_URL=http://127.0.0.1:8788 claude
wang proxy stats                             # requests, $, cache hit, bytes removed per client
```

For opencode and other OpenAI-compatible clients, set the provider's base URL to `http://127.0.0.1:8788/openai/v1`.
Requests go to OpenRouter by default; `--openai-upstream` changes that. Example for opencode's bundled OpenRouter provider:

```json
{"provider": {"openrouter": {"options": {"baseURL": "http://127.0.0.1:8788/openai/v1"}}}}
```

Options:
- `--cap-chars N`: characters kept per tool output; default 40000, and 0 turns trimming off. Claude Code already caps its own output at about 30k, so this only affects outliers.
- `--no-dedup`, `--no-cache-hints`: turn off the other two rewrites.
- `--daily-budget USD`: refuse calls once the day's spend reaches this.
- `--anthropic-upstream URL`: send Anthropic API requests somewhere else.

Keep the rules fixed during a conversation. If they change mid-conversation, earlier messages are rewritten differently and the provider may reject old thinking blocks. When that happens the proxy retries once without them.

## Install

```
curl -fsSL https://github.com/hungnguyen1607/wang/releases/latest/download/install.sh | sh
wang
```

Releases are built by `.github/workflows/release.yml` on a `v*` tag, using `scripts/package.sh`. Until the first release, build from source with `cargo build --release` and run `target/release/wang`.

## Use wang from Claude Code / Claude Desktop

```
claude mcp add wang -- wang mcp                # Claude Code
```

For Claude Desktop: Settings → Developer → Edit Config, then add the following and restart the app:

```json
{"mcpServers": {"wang": {"command": "/full/path/to/wang", "args": ["mcp"]}}}
```

Then just ask, e.g. "use wang's VM to try these three fixes and keep the one that passes the tests". You can also pick a workflow from the slash menu (`/mcp__wang__research`, `/mcp__wang__code`, `/mcp__wang__quick`).

- Each Claude session gets its own browser and VM, which persist across its calls and are closed when the session ends.
- `[mcp] allow_privileged = true` lets shell commands write files and use the network. Claude's own approval prompt is then the only check.
- wang's own agent isn't involved here: Claude is the agent. Use `wang chat` or Telegram for wang's agent.

## Micro-VMs

You need KVM (`/dev/kvm`), Docker and `squashfs-tools` (`mksquashfs` 4.6 or newer). No root is needed.

```
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-unknown-linux-gnu -p wang-vm-agent
wang vm setup                                  # Firecracker + kernel (checksums pinned), image from python:3.12-slim, base snapshot
wang vm run python3 -c 'print(42)'             # a throwaway VM
wang vm try --setup "pip install requests" --score "python3 score.py" -- "python3 a.py" "python3 b.py"
wang vm bench                                  # boot / restore / fork / memory numbers
```

`[vm]` in the config sets `vcpus`, `memory` (guest RAM, default 512M), `domains` (the guest's allow-list, default `["*"]` public sites only) and `max_vms`. Use `wang vm setup --image <docker image>` to build from another image. Checkpoints are full memory snapshots on disk under `~/.cache/wang/vm`.

## Browser

The browser needs a Chrome that can run inside bubblewrap. The snap Chromium can't. The easiest option is Playwright's headless shell:

```
npx playwright install chromium-headless-shell
wang browse https://news.ycombinator.com      # what the agent sees
wang browse --stats URL...                    # sizes, load times, memory
```

`[browser]` in the config sets these:
- `domains`: the pages' allow-list. The default `["*"]` allows any public site. Private and local addresses need an exact entry.
- `memory`: RAM cap per browser (default 1G).
- `max_browsers`: how many run at once (default 2).
- `idle_timeout`: how long an unused browser stays open (default 5m).

The browser only runs when the sandbox works. If the sandbox is off or broken, the agent gets no browser tool.

## Sandbox config

```toml
[sandbox]
enabled = true
read = ["~"]              # visible read-only (secrets always hidden)
write = []                # e.g. ["~/notes"]: writes reach real files
overlay = []              # copy-on-write: writes discarded after each command
domains = []              # shell network allow-list, e.g. ["api.github.com", "*.pypi.org"]
memory = "512M"
cpus = 1.0
pids = 128

[web_fetch]
domains = ["*"]           # private/local addresses refused unless listed exactly
```

Needs `bwrap` (bubblewrap, preinstalled on Ubuntu; `sudo apt install bubblewrap` otherwise) and cgroups v2 with a systemd user session.

### `jail`: the sandbox on its own

```bash
jail run --ro ~/notes --rw ~/notes/out --allow-net api.github.com --memory 256M -- python3 skill.py
jail run --manifest skill.toml --report -- ./skill.sh
jail selftest            # escape test suite
jail bench               # overhead per call
```

Manifest:

```toml
[files]
read = ["~/notes"]
write = ["~/notes/out"]
[network]
domains = ["api.github.com"]
[limits]
memory = "256M"
pids = 64
timeout = "30s"
```

## Commands

| | |
|---|---|
| `wang chat [message]` | interactive, or one-shot with a message; `--session NAME`, `--new` |
| in chat | `/new` `/good` `/bad` `/trace` `/cost` `/quit` |
| `wang cost [today\|week\|month]` | tokens, cache-hit %, $ per task |
| `wang trace [id]` | step-by-step timeline (default: latest task) |
| `wang rate good\|bad [id]` | 👍 / 👎 a task |
| `wang stats [today\|week\|month\|all]` | agent health (default: week) |
| `wang notify test` | send a test ntfy alert |
| `wang approve <id>` / `wang deny <id>` | answer an approval request |
| `wang memory [fact]` | list saved facts, or add one |
| `wang eval [--live] [--routing] [--only ID]` | run the eval suite (replay by default) |
| `wang bench [--levels 1,10,100] [--no-sandbox] [--latency-ms N]` | scale bench against a fake model server |
| `wang browse URL... [--part N] [--stats]` | open pages in the sandboxed browser and print the compact view |
| `wang vm setup\|run\|try\|bench` | Firecracker micro-VMs: build, run a command, try approaches in parallel, measure |
| `wang` | first run: guided setup; after that: chat (starts the daemon if needed) |
| `wang setup` / `wang doctor` | guided setup / what works on this machine and how to fix the rest |
| `wang schedule add WHEN TASK` / `list` / `rm N` | tasks wang does on a schedule |
| `wang mcp` | serve wang's tools to Claude Code / Claude Desktop (MCP over stdio) |
| `wang local setup\|ask\|bench` | the local model: download (checksums pinned), ask it something, measure speed and summary quality |
| `wang proxy [--port N] [--daily-budget USD]` / `wang proxy stats [today\|week\|month]` | token proxy for Claude Code / opencode, and what it saved |
| `wang service install` | systemd user unit |

Data: `~/.local/share/wang/wang.db` (ledger + traces). Logs: stderr / journald, `WANG_LOG=debug` for more.

## Layout

```
crates/wang-tokens   pricing, output caps, cost ledger, stats (library)
crates/wang-jail     sandbox library + `jail` CLI (bubblewrap, cgroups v2, seccomp, egress proxy)
crates/wang-trace    spans + timeline rendering (library)
crates/wang-daemon   agent loop, model clients, tools, Channel trait + Unix-socket channel, daemon
crates/wang-evals    eval suite: recorded tasks, cassette replay, run comparison
crates/wang-bench    scale bench: fake model server + N agents at once
crates/wang-browser  headless Chrome in the jail: own CDP client, compact page view, browser pool
crates/wang-vm       Firecracker micro-VMs in the jail: boot, snapshot, fork, rollback, try-N-keep-best
crates/wang-vm-agent PID 1 inside the VMs (static binary): RAM overlay root, commands over vsock
crates/wang-proxy    token proxy for other agents + the standalone `wang-proxy` binary
crates/wang-cli      the `wang` binary
```

## Tests

`cargo test`: 128 tests, including the agent loop against a scripted model, OpenRouter SSE parsing
(split UTF-8, tool-call fragments, reasoning fragments, mid-stream errors, truncation), a real
Unix-socket round trip, loop detection, stats on a mixed set of tasks, the sandboxed tools against the real jail, a small bench ramp against the fake model server, and the Telegram channel against a fake Bot API (strangers ignored, approval taps, message rollover, no token in errors), and the token proxy against a fake provider (byte-for-byte streaming, rewrites stay append-only across turns, auth and query passthrough, budget refusal, thinking-mismatch retry), and the browser: real Chrome in the real jail against a local site (view contents, clicking, forms, back, dialogs, paging, allow-list blocks, a memory hog stopped at the cap, pool caps and eviction, the agent's browser tool), and real micro-VMs (isolation from host files and services, allow-listed guest network, clock after restore, forks, try-keep-best, checkpoint/rollback, a memory bomb, the agent's vm tool), the MCP server (handshake, tools, prompts, the privilege gate), the workflows (classification, guidance, tool gating), orchestration (parallel workers, dependency results, retry and re-plan, fallbacks, per-worker micro-VMs), and schedules (plain-English times, the store, running and delivering a due task, skipping long-missed runs, the agent's schedule tool). `jail selftest` runs the escape suite.
