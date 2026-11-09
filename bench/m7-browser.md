# M7 browser runtime: results

2026-11-08. Laptop: 16 cores, 30 GB RAM. Chrome for Testing headless shell 156 (Playwright build 1248).

## What runs

```
agent ── browser tool ──► Pool: one browser per conversation, max N, idle ones closed after 5 min
                             └─ jail: cgroup (1 GiB RAM, no swap, 2 CPUs, 2048 pids), namespaces, seccomp
                                  └─ chrome --remote-debugging-pipe ◄── wang's own CDP client (fd 3/4)
                                       └─ every request ─► jail egress proxy ─► domain allow-list
```

- **Control over a pipe, not a port.** Nothing else on the machine (or in the jail) can connect to
  the browser. The CDP client is about 200 lines (`cdp.rs`), with no WebSocket or CDP library.
- **Fresh profile every launch** in the jail's private /tmp, with no cookies or logins. The user's real
  browser profiles are hidden from every jail.
- **Chrome's own sandbox is off** (`--no-sandbox`). The jail disables nested user namespaces, which
  Chrome's sandbox needs. The jail is the boundary instead: a compromised renderer finds only an empty
  home, read-only /usr, no secrets, and a network that only reaches allowed domains.
- **Only http(s) URLs.** `file://`, `chrome://`, `javascript:` and `data:` navigations are refused.
  Downloads are denied, and alert/confirm dialogs are dismissed automatically.

## The compact page view

The agent never sees HTML. `view.js` walks the rendered DOM and prints:
- the visible text, with `#` for headings, `-` for list items and `|` between table cells;
- every link, button, field, dropdown and checkbox as `[n ...]`.

Actions refer to those numbers. Same-site links carry no URL, since the agent clicks them by number and
the view's header shows where it landed. Links to other sites show only the host.

The view skips:
- anything hidden;
- scripts and styles;
- repeated menu lines;
- form labels, which are folded into their fields.

It handles `display: contents`, web components (shadow DOM and slots) and iframes (marked). Long pages
come in parts of 12k chars (~3k tokens).

Real pages, one browser, `wang browse --stats`:

| page | HTML | plain text | **view** | HTML ÷ view | elements |
|---|---|---|---|---|---|
| example.com | 2.0 KB | 0.9 KB | **1.3 KB** | 1.5× | 1 |
| news.ycombinator.com | 34.6 KB | 4.1 KB | **6.4 KB** | 5.4× | 230 |
| Wikipedia: Rust | 1387 KB | 76.9 KB | **96.7 KB** | 14.3× | 1684 |
| github.com/tokio-rs/tokio | 393 KB | 9.8 KB | **12.1 KB** | 32.5× | 241 |
| docs.rs/tokio | 60.7 KB | 16.9 KB | **17.6 KB** | 3.4× | 115 |
| MDN: HTTP status codes | 151 KB | 16.6 KB | **21.4 KB** | 7.1× | 358 |
| bbc.com/news | 332 KB | 8.3 KB | **6.6 KB** | 50.6× | 121 |
| lobste.rs | 54.6 KB | 3.2 KB | **5.0 KB** | 10.9× | 201 |

What this shows:
- The view is 1.5–50× smaller than the HTML.
- It's about the size of the page's plain text (0.8–1.6×) while also holding every clickable element.
- Showing no URL for same-site links halved the view on link-heavy pages: Hacker News went from 12.9 KB
  to 6.4 KB and Wikipedia from 177 KB to 97 KB.

Two bugs found on real pages, both now covered by the test fixture:
- MDN's article sits in a `display: contents` `<main>`, which `checkVisibility()` reports as invisible.
- Slotted content inside web components was skipped.

## Speed and memory

| | |
|---|---|
| launch (jail + Chrome + first tab) | ~1.1–1.3 s |
| page load (to load event + network quiet, ≤1.5 s wait) | 1.1–3.9 s on the pages above |
| view of a page | <100 ms (Wikipedia, 1684 elements) |
| RAM per idle browser (cgroup `memory.current`) | ~75 MB |
| after loading Hacker News | ~88 MB |
| one tab after 8 heavy pages | ~410 MB (under the 1 GiB cap) |
| 4 browsers launched at once | all ready in 1.4 s, ~350 MB total after a page each |

Reproduce:
- `wang browse --stats URL...`
- `cargo run --release -p wang-browser --example pool_bench -- 4 https://news.ycombinator.com`. This
  needs the `jail` binary built first: `cargo build --release -p wang-jail --bin jail`.

## Limits enforced (tests)

- **Memory cap:** a page that allocates without end is stopped at the cgroup cap (512 MiB in the test).
  The agent is told the page crashed, and a fresh tab works afterwards.
- **Pool:**
  - the same conversation reuses its browser;
  - when the pool is full, the least-recently-used *idle* browser is closed;
  - when every browser is busy, a new one is refused instead of going over the cap.
- **Free memory:** no new browser starts when the machine has less than `min_free` (768 MiB) available.
- **Allow-list:** pages on other hosts or ports are blocked by the jail, and the agent is told which
  host was blocked. Plain-http pages are blocked too; the proxy returns a 403 page, which `open`
  reports as an error.

## Two sandbox bugs found and fixed while building this

1. **Jailed commands were killed after ~10 s.** This came from M5's bench fix #2, which moved bwrap
   spawning to tokio's blocking pool. bwrap's `--die-with-parent` uses PR_SET_PDEATHSIG, which fires
   when the *thread* that forked it exits. Blocking-pool threads exit after 10 s idle, so `jail run --
   sleep 15` died with exit 137 at 10.0 s. Any shell tool call longer than 10 s was affected, though its
   limit is 60 s. The bench's short commands never showed it.
   - Fix: spawning now happens on long-lived spawner threads (one per core) that last as long as the
     `Jail`.
   - Regression test: tokio's thread keep-alive is set to 100 ms. The test fails with the old code and
     passes with the fix.
   - Bench after the fix: 34 tasks/s at 250 agents (M5: 30), stall p95 296 ms (M5: 315).
2. **Two `Jail`s in one process shared a run directory.** Dropping one deleted the seccomp program and
   proxy sockets the other was using. Each `Jail` now gets its own directory.

## Not built yet

- **Screenshots.** The view is text-only; a screenshot action for vision models could come later.
- **Approval for submitting forms.** The browser has no logins or cookies, so it can't act as you. If
  logged-in browsing is added, form submits should need approval.
- **Sharing one browser across conversations** (browser contexts). This would use less RAM than one
  browser per conversation, but the conversations would share a jail and an allow-list.
