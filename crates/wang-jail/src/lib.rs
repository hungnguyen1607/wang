//! wang-jail: run untrusted commands with only the files, domains and resources they declare.
//!
//! ```text
//! cgroup (RAM, no swap, CPU, pids; kill-all)
//!   └─ bubblewrap: user/mount/pid/net/ipc/uts namespaces, fresh root, secrets hidden
//!        └─ seccomp deny-list
//!             └─ loopback only ── declared domains ──► egress proxy (outside) ──► internet
//! ```

pub mod bench;
pub mod cgroup;
pub mod helper;
pub mod policy;
pub mod proxy;
pub mod run;
pub mod seccomp;
pub mod selftest;

pub use policy::{Bind, Limits, Manifest, Policy};
pub use run::{Io, Jail, JailResult};
