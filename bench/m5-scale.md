# M5 scale bench: 1 → 1000 agents at once

`wang bench`, 2026-11-07. Laptop: 16 cores, 30 GB RAM (~4.5 GB free, swap full), NVMe ext4, Ubuntu, kernel 7.0.

## Setup

- **Fake model server** (`crates/wang-bench/src/fake.rs`): speaks OpenRouter's streamed chat-completions
  protocol on 127.0.0.1, on its own thread. It keeps no per-agent state: the step is the number of assistant
  messages in the request. It replays a fixed 3-step script: `shell wc -l data.txt` → `read_file data.txt` →
  the answer. 100 ms before each reply. $0.
- **Agents**: real `Agent`s with the real OpenRouter client, HTTP, SSE parser and tools. Each agent gets its
  own fixture folder and jail policy (read-only, no network). All share one ledger + trace DB on disk, on the
  single-threaded runtime the daemon uses. 2 tasks per agent per level, each checked for the right answer.
- **Measured per level**: tasks/s, model steps/s, task latency, model-call and tool-call latency (from the
  trace spans), **stall** (how late a 10 ms timer fires: blocking work on the event loop), RSS, and system
  memory per agent. The ramp stops early on >5% failed tasks or when free memory would drop below 1 GB.

## Final results

Sandbox on (the real configuration):

```
  agents  tasks  fail  task/s  step/s  task p50  task p95  model50  tool50  tool95 stall95  rssMB MB/agt
      1      2     0     2.3     7.0      436      436   102.2    64.1    68.8     1.5     11   13.0
     10     20     0    15.5    46.6      591      677   103.0   151.8   203.7     2.3     13    2.2
     50    100     0    29.7    89.2     1547     1854   121.9   597.6   849.3    22.5     24    2.1
    100    200     0    31.7    95.2     2768     3842   122.7  1218.7  1869.0    23.9     38    1.3
    250    500     0    30.4    91.2     7603     9877   245.9  3433.1  4690.6   125.9     74    1.0
    500   1000     0    29.3    87.8    16516    21830  2508.5  4097.9  8628.2  2339.0    103    0.2
   1000   2000     0    26.1    78.2    37966    43923  5207.0  9391.3 19396.8  5479.9    156    0.1
```

Sandbox off (`--no-sandbox`, to see the daemon's own limit):

```
  agents  tasks  fail  task/s  step/s  task p50  task p95  model50  tool50  tool95 stall95  rssMB MB/agt
      1      2     0     3.1     9.3      323      323   102.2    11.0    13.5     1.2     10    7.6
     50    100     0   104.8   314.5      440      485   118.1    11.6    73.3    19.9     18    0.0
    100    200     0   163.0   488.9      549      674   135.7    33.5   161.9    79.8     26    0.0
    250    500     0   195.8   587.5     1150     1383   251.8    77.5   449.7   225.4     48    0.2
    500   1000     0   190.0   569.9     2288     3790   642.5    90.0  1283.8   459.7     85    0.2
   1000   2000     0   184.4   553.1     4627     7742  1316.9   105.9  2632.7   991.6    159    0.1
```

Headline:
- **1000 agents at once, 0 failed tasks**, sandbox on. The daemon grows from 11 MB to 156 MB RSS, ~0.15 MB per agent.
- Peak **~95 model steps/s (~32 tasks/s) sandboxed**, ~590 steps/s unsandboxed.
- Two repeat runs matched within a few %.

## Bottlenecks found, in order

### 1. SQLite fsync on every commit (fixed: 10×)

First run with the DB on disk: throughput capped at **17 tasks/s** and the event loop stalled for **2 s** at
200 agents. With the DB on tmpfs the same run reached 215 tasks/s. strace: **25 fsyncs per task**. Every
ledger row and every trace span start/end is its own commit, and WAL's default `synchronous=FULL` fsyncs
each one on the runtime thread.

Fix: `PRAGMA synchronous=NORMAL` (ledger + traces). In WAL mode that's corruption-safe; a power cut can lose
the last few writes, which is fine for a cost ledger and traces.

| sandbox off, DB on disk, 200 agents | task/s | task p50 | stall p95 |
|---|---|---|---|
| before (`FULL`) | 17.1 | 10.6 s | 1989 ms |
| after (`NORMAL`) | 170.9 | 1.1 s | 139 ms |

### 2. Jail setup on the event loop (fixed: stalls 5× lower)

Sandboxed, the runtime thread sat at ~98% CPU, ~80% of it in the kernel, and was sometimes parked in
`skb_wait_for_more_packets`. Per call it did cgroup mkdir + 4 writes (0.6 ms), fork/exec (0.6 ms, and
the parent waits until the child has exec'd, which takes far longer when hundreds of bwraps compete for
CPU), and kill + rmdir (0.3 ms). All of that ran synchronously and serialized every jailed call.

Fix: cgroup create, spawn and teardown run on tokio's blocking pool (`wang-jail/src/run.rs`, `cgroup.rs`).

| sandbox on, 250 agents | stall p95 | model-call p50 |
|---|---|---|
| before | 1593 ms | 2014 ms |
| after | 315 ms | 586 ms |

Throughput didn't change (see 3).

> **Correction (M7, 2026-11-08):** this fix had a bug. bwrap's `--die-with-parent` fires when the *thread*
> that forked it exits, and blocking-pool threads exit after 10 s idle, so jailed commands longer than
> ~10 s were SIGKILLed. Spawning now runs on long-lived spawner threads instead. Throughput afterwards:
> 34 tasks/s at 250 agents. Details in [m7-browser.md](m7-browser.md).
>
> **Correction (M8):** fix #3's async "ready" read also had a bug. It read bwrap's info pipe once and
> closed it, so under load about 1–2 of 64 jails died with SIGPIPE (exit 141). The "0 failures" above
> didn't catch this, because the fake model's answer doesn't depend on tool output. Fixed, and the bench
> now has a `toolerr` column: 500 agents, 0 tool errors, ~31 tasks/s. See [m8-vm.md](m8-vm.md).

### 3. Fork contention at high counts (mitigated)

At 500–1000 agents the runtime thread ran at ~15–20% CPU, mostly blocked in `futex_do_wait` and
`lock_mm_and_find_vma`. Each `fork()` of the multi-threaded wang process holds its memory-map lock while
it copies page tables (and glibc's fork takes the malloc arena locks), so with hundreds of forks in
flight, every thread in the process waited. Also, every in-flight jail held a blocking-pool thread just to
wait for bwrap's "ready" byte.

Fix: at most one concurrent jail spawn per core (semaphore), and the "ready" wait is now an async pipe read.
At 1000 agents: 21.6 → 26.1 tasks/s, stall p95 7.6 s → 5.3 s.

Not fully fixed: with this many processes in flight, the event loop still stalls for seconds at 500+.

### 4. The ceiling: the kernel creates ~75 sandboxes/s (not fixed, needs a design change)

Plain bubblewrap on this laptop, no wang involved (`bwrap … /usr/bin/true`, 300 runs):

| namespaces | serial | 16 parallel |
|---|---|---|
| user | 36/s | 78/s |
| user + pid | 37/s | 77/s |
| user + net | 33/s | 67/s |
| all | 35/s | 78/s |

Network namespaces aren't the problem: namespace/mount setup itself is serialized in the kernel at
~75/s while using only ~2 cores. With 2 jailed calls per task, that's ~37 tasks/s max, and wang
reaches ~32. No amount of daemon tuning gets past this.

## What's next (not built)

1. **Warm session jails**: one long-lived jail per agent session, with commands run inside it, instead of a
   fresh jail per command. This removes the per-call ~45 ms and the 75/s ceiling, and reduces forking to
   one per session. Trade-off: files in the jail's private `/tmp` and leftover processes persist between
   calls within one session, and limits become per-session instead of per-command. Needs its own escape tests.
2. **Spawn without fork**: `clone3(CLONE_VM | CLONE_VFORK | CLONE_INTO_CGROUP)` (or posix_spawn plus a tiny
   launcher) instead of fork + `pre_exec`. No page-table copy and no lock contention, so the event loop
   should stop stalling at 500+.
3. **Unsandboxed ceiling (~590 steps/s, one thread)**: not worth chasing. With real model latency
   (seconds per call), the sandbox ceiling is hit long before this.

Reproduce: `wang bench` (default levels 1,10,50,100,250,500,1000), `wang bench --no-sandbox`. Results
go to `bench/runs/<time>/report.json`.
