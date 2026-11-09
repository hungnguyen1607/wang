# M8 micro-VMs: results

2026-11-08. Laptop: 16 cores, 30 GB RAM, NVMe. Firecracker v1.17.0, guest kernel 6.1.155 (Firecracker CI
build), image `python:3.12-slim` (37 MiB squashfs). VMs have 2 vCPUs and 512 MiB of guest RAM.

## What runs

```
wang ── vsock (Unix socket) ──► wang-vm-agent (PID 1 in the guest): exec / read / write
  └─ wang jail: cgroup (guest RAM + 128 MiB, no swap), namespaces, seccomp; only /dev/kvm passed in
       └─ firecracker (its own seccomp) ── KVM ──► guest
            ├─ root: read-only squashfs built from a Docker image, RAM overlay on top
            ├─ no network device; 127.0.0.1:3128 ── vsock ──► host egress proxy (domain allow-list)
            └─ virtio-rng; VMGenID reseeds the guest RNG when a snapshot is restored
```

- **Images from any Docker image:**
  - `docker export | mksquashfs -tar`, with no root needed and file ownership kept;
  - the static guest agent is added as `/wang-agent`;
  - `wang vm setup --image <image>`; the default is Python 3.12 on Debian.
- **The guest writes only to RAM.** The image is shared read-only by every VM, and all writes go to a
  tmpfs overlay in guest memory. So a memory snapshot is the whole machine, and a restore brings back
  memory and files together.
- **Fixed paths inside the jail.** Every VM sees `/fc/*` (assets), `/vm` (its own sockets) and `/snap`
  (the snapshot it came from). Snapshots record those paths, so one snapshot restores into N VMs at
  once without their sockets colliding. This is the jailer's chroot trick, done by wang's own jail.
- **Clock:** a restored VM wakes with the clock from snapshot time. The host sends its time with each
  request, and the agent steps the guest clock when it's off by more than 1 s.
- **Network:** each VM can reach only the domains on its own allow-list, through wang's egress proxy.
  Private and LAN addresses are refused. The VM has no route to the host's services.

## Results (`wang vm bench --clones 8`)

| | |
|---|---|
| cold boot (spawn → kernel → agent answers) | **308 ms** median (304–326) |
| restore from a snapshot → agent answers | **77 ms** median (73–80) |
| snapshot (pause + save 512 MiB) | 1.7–2.8 s; the memory file is the full guest RAM on disk |
| fork 8 VMs from one snapshot at once | **244–328 ms** until all 8 agents answer |
| memory of 8 idle forks | **42 MiB PSS total, 5 MiB per VM** (each has 512 MiB of guest RAM) |
| try 4 approaches in parallel (fork, run, score, keep best) | fork 148 ms, all scored in **418 ms** |

The plan's goal was booting in under 1 s. A cold boot takes 0.3 s, and new VMs start in under 0.1 s
because they restore the base snapshot saved by `wang vm setup`. Most of the boot time had been probing
a PS/2 keyboard controller that a VM doesn't have. Firecracker's recommended boot arguments
(`i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd`) plus `quiet` cut it from 1.25 s to 0.25 s.

### Memory sharing

- **Page cache (works out of the box):** forks map the snapshot's memory file copy-on-write, so
  unchanged pages exist once on the host. 8 idle forks cost 42 MiB in total.
- **KSM (enabled, but depends on a root-only setting).** wang enrolls each Firecracker process with
  `PR_SET_MEMORY_MERGE`; `ksm_merge_any: yes` is verified in `/proc/<pid>/ksm_stat`. But this laptop's
  kernel scans only 2 MB/s (`pages_to_scan=100`, `sleep_millisecs=200`). After 8 forks each allocated
  96 MiB of identical data, nothing was merged in 15 s; at that rate it takes about 15 minutes. To make
  KSM useful:
  `echo 5000 | sudo tee /sys/kernel/mm/ksm/pages_to_scan; echo 20 | sudo tee /sys/kernel/mm/ksm/sleep_millisecs`.
  The bench prints the scan rate and this hint.

## Tests (real VMs, in `cargo test`)

- **Isolation:**
  - the guest runs as root but sees none of the host's files;
  - its only interface is `lo`;
  - TCP to the host's services, 10.0.2.2 and the LAN fails.
- **Allow-list:** with `domains = ["pypi.org"]`, pypi.org works through the proxy. example.com gets a 403,
  and the blocked host is recorded.
- **Files and clock:** writes land in the RAM overlay, and file read/write works. The guest clock is
  within 2 s of the host's after a restore.
- **Timeouts:** a timed-out command is killed, and the VM keeps working.
- **Forks:**
  - they start from the snapshot and don't see each other's writes;
  - 3 idle forks use less than ¾ of one VM's RAM.
- **Try:**
  - the highest score wins, and a failed attempt scores 0;
  - the kept VM holds the winner's files;
  - more forks than `max_vms` are refused.
- **Sessions:**
  - checkpoint and rollback;
  - each conversation has its own VM;
  - after a try, the session continues from the best copy;
  - a memory bomb in the guest fails without affecting the host, and the conversation keeps working;
  - `reset`.
- **Agent tool:** `vm` run / checkpoint / try / rollback through the same API the agent uses.

## Two more sandbox bugs found (both fixed, with regression tests)

1. **Jailed commands randomly died with exit 141 (SIGPIPE).** bwrap writes its `--info-fd` message in
   several writes, and wang read the first chunk and closed the pipe. When the reads interleaved, which
   is likely under load, bwrap's next write killed it.
   - This had been there since M5's switch to an async "ready" read. It caused the one-off M7 test
     failure and failed checks in the escape suite under load (fail-closed: the sandboxes died rather
     than leaking).
   - The M5 bench didn't notice: the fake model's answer doesn't depend on tool output, so tasks still
     "passed".
   - Fix: drain the pipe until bwrap closes it.
   - Test: 64 jails at once. The old code fails 1–2 of them on every run; the fix passes.
   - The bench now has a `toolerr` column and counts tool errors as failures. 500 agents, sandboxed:
     0 tool errors.
2. **The same SIGPIPE in the new `spawn_piped`.** It closed the info pipe right away. I found it while
   getting Chrome to start in M7, and fixed it the same way.

## Not built / limits

- **No graphical desktop.** The plan said "VM desktop". This is a Linux machine for jobs (shell, Python,
  packages), and the agent drives it through commands, not a screen. A GUI (Xvfb + VNC, or the M7
  browser inside the VM) can be layered on later.
- **No host files are shared into the VM.** The agent creates files there with commands. A shared
  workspace (a virtio-fs or block device) is future work, and it would need its own policy.
- **Snapshots are full-size on disk** (the guest RAM size). Diff snapshots would make checkpoints cheaper.
- **Restored clones start from identical memory.** VMGenID reseeds the kernel RNG. User-space random
  state captured in the snapshot (e.g. a running Python's `random`) is still shared by the clones.
- **Firecracker runs under wang's jail, not Firecracker's own jailer,** which needs root. The layers are
  bwrap namespaces, a cgroup cap, wang's seccomp deny-list, Firecracker's own seccomp filters, and KVM.
