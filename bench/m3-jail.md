# M3 sandbox results

Machine: Ryzen 7 5700U, Ubuntu 26.04, kernel 7.0.0-38-generic, bubblewrap 0.11.1. Date: 2026-11-07.

## Escape suite (`jail selftest`)

```
escape attempts
  BLOCKED secrets are hidden (~/.ssh, ~/.gnupg, wang's .env and DB, browser profiles, ...)
      7 secret paths present on this machine, 0 readable
  BLOCKED declared-hidden canary can't be read
      hellocat: /tmp/wang-canary-1097964/secret/key: No such file or directory
  BLOCKED read-only home can't be written
      touch: cannot touch '/home/hung/.wang-jail-write-test': Read-only file system rc=1
  BLOCKED host /tmp is not reachable (jail /tmp is private)
      rc=0
  BLOCKED overlay writes stay in the jail
      inside: "changed", host after: "hello"
  BLOCKED no network without an allow-list (direct TCP to 1.1.1.1)
      blocked: [Errno 101] Network is unreachable
  BLOCKED undeclared domain refused by the egress proxy
      curl: (7) CONNECT tunnel failed, response 403 000 · proxy blocked: ["evil.invalid:443"]
  BLOCKED proxy can't be bypassed (direct connections still fail)
      blocked: [Errno 101] Network is unreachable curl: (6) Could not resolve host: example.com rc=6
  BLOCKED wildcard allow-list can't reach the host or LAN (localhost, 127.0.0.1)
      proxy blocked: ["127.0.0.1:80 (private address)", "localhost:443 (private address)"]
  BLOCKED fork bomb is contained by the process limit
      62 forks refused at pids.max=64, finished in 1.1s
  BLOCKED memory bomb is killed at the RAM limit (no swap)
      oom kills: 1, peak 128 MiB, exit 137
  BLOCKED ptrace is blocked (seccomp)
      ptrace -1 Operation not permitted
  BLOCKED nested user namespaces are blocked
      unshare: unshare failed: Operation not permitted rc=1
  BLOCKED mounting is blocked
      mount: /tmp/m: must be superuser to use mount. dmesg(1) may have more information after failed mount system call. rc=32
  BLOCKED host processes are invisible (PID namespace)
      5 processes visible
  BLOCKED wang's daemon socket and the user runtime dir are not reachable
      ls: cannot access '/run/user/1000': No such file or directory rc=0

normal work
  OK      declared write path is writable
      
  OK      declared domain is reachable through the proxy
      200
  OK      normal commands work (ls home, python)
      42 ok

16/16 escapes blocked · 3/3 normal checks ok
```

## Overhead (`jail bench -n 50`)

```
jail bench — `true`, 50 runs each

  plain spawn                  p50    1.64 ms   p95    2.23 ms   max    2.71 ms
  jailed                       p50   46.18 ms   p95   76.08 ms   max   88.81 ms
    of which sandbox setup     p50   16.70 ms   p95   48.03 ms   max   54.54 ms
  jailed + egress proxy        p50   48.29 ms   p95   54.05 ms   max   58.37 ms

  overhead per call (p50): 44.54 ms (no network), 46.65 ms (with proxy)
```

Bare `bwrap` with the same namespaces costs ~31 ms on this kernel; wang-jail's cgroup + seccomp + helper add ~13 ms on top.
