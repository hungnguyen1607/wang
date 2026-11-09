# M9 local model: results

2026-11-08. llama.cpp b12200 (official CPU build, checksum pinned), Qwen2.5-1.5B-Instruct Q4_K_M (1.04 GiB,
checksum pinned). Laptop CPU, run while the desktop was busy (load average ~14 on 16 cores: Firefox and
Claude Desktop).

## What it does

- **`llama-server` runs inside wang's jail:**
  - no network namespace access;
  - only its binaries and the model file are visible, read-only;
  - RAM capped at 3 GiB;
  - it listens on a Unix socket in a private 0700 directory, so nothing else on the machine can use it.
- **Starts on first use and stops after 10 minutes idle,** so it holds no RAM while unused.
- **Its one job:** when a conversation gets long, it writes the history summary (M4's compaction)
  instead of the main model. The call is recorded in the ledger at $0. If the local model fails, or the
  history doesn't fit its context, the main model does it as before.
- **It gets its own fixed-section template** (Goal / Decisions / Facts / Done / Pending / Preferences),
  because small models follow structure better than open-ended instructions.
- **Commands:** `wang local setup`, `wang local ask …`, `wang local bench`.

## Results (`wang local bench`)

| | 4 threads | 8 threads |
|---|---|---|
| cold start in the jail | 3.4 s | 3.4 s |
| summary of a 1.3k-token conversation | 39 s | 23 s |
| prompt speed | 50 tok/s | 75 tok/s (llama-bench outside the jail: 72) |
| generation speed | 17 tok/s | 21 tok/s |
| warm repeat (prompt cache) | 6.6 s | 5.7 s |
| memory (cgroup) | 1.18 GiB | 1.18 GiB |
| key facts kept (of 9) | 78% | 78% |
| cost | $0 (Haiku 5.5: $0.0002) | $0 |

The jail costs nothing measurable here; llama.cpp gets the same speed outside it. This laptop is simply
slow at prompt processing, especially with the desktop busy.

**Quality.** With the open-ended prompt, the summary said the orders and admin work was finished when it
was still pending. With the template, it invented two versions that appear nowhere in the conversation
("PostgreSQL 14.2", "pytest 8.2"). It also listed the Decimal fix as pending when it was done, and dropped
"use SQLAlchemy 2.0" and the user's `pytest -x` preference.

## Decision: built, tested, **off by default**

The plan's rule is to keep an optimization only if quality holds. This one doesn't:
- It saves about $0.0002 per summary on Haiku 5.5, or about $0.005 on Opus 5.5.
- It adds 20–40 s inside a turn on this laptop.
- It can write false facts into the agent's memory of the conversation.

Turn it on with `[local] enabled = true` after `wang local setup`. It may be worth it with a faster
CPU or GPU, or a larger model.

What would change the decision:
- **A bigger model** (Qwen2.5-3B or 7B): better recall, but 2–4× slower on this CPU.
- **A job that tolerates small mistakes:** classifying tasks for M10's task-shaped workflows ("research,
  code, or quick edit?"), titles, or picking which tool output to trim. These are short prompts, so they're
  fast even here.
- **Live evals with a real API key,** to measure whether local summaries change task success.

## Tests

- **The real local model** (skipped unless set up): a long conversation is compacted by the local model.
  The main model sees only the real turn, and the summary is recorded in the ledger at $0.
- **Fallback:** a configured local model that can't start falls back to the main model, and the turn still
  works.
- **Bench fixture:** every key fact is in the sample conversation, and recall scoring works.
