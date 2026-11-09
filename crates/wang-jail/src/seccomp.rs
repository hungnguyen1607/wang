//! Seccomp filter: a deny-list of kernel attack surface (Docker-style).
//!
//! An allow-list can't work for "run any shell command" (bash, python, git, ... each need a
//! different syscall set), so the jail allows by default and returns EPERM for syscalls a
//! tool never needs: tracing other processes, kernel modules, BPF, keyrings, mounting,
//! namespaces, io_uring, clock/reboot/swap. Nested user namespaces are also disabled by
//! bubblewrap (`--disable-userns`).

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

pub const DENIED: &[(&str, i64)] = &[
    ("ptrace", libc::SYS_ptrace),
    ("process_vm_readv", libc::SYS_process_vm_readv),
    ("process_vm_writev", libc::SYS_process_vm_writev),
    ("kexec_load", libc::SYS_kexec_load),
    ("kexec_file_load", libc::SYS_kexec_file_load),
    ("init_module", libc::SYS_init_module),
    ("finit_module", libc::SYS_finit_module),
    ("delete_module", libc::SYS_delete_module),
    ("bpf", libc::SYS_bpf),
    ("perf_event_open", libc::SYS_perf_event_open),
    ("userfaultfd", libc::SYS_userfaultfd),
    ("keyctl", libc::SYS_keyctl),
    ("add_key", libc::SYS_add_key),
    ("request_key", libc::SYS_request_key),
    ("mount", libc::SYS_mount),
    ("umount2", libc::SYS_umount2),
    ("pivot_root", libc::SYS_pivot_root),
    ("fsopen", libc::SYS_fsopen),
    ("fsconfig", libc::SYS_fsconfig),
    ("fsmount", libc::SYS_fsmount),
    ("fspick", libc::SYS_fspick),
    ("move_mount", libc::SYS_move_mount),
    ("open_tree", libc::SYS_open_tree),
    ("mount_setattr", libc::SYS_mount_setattr),
    ("unshare", libc::SYS_unshare),
    ("setns", libc::SYS_setns),
    ("swapon", libc::SYS_swapon),
    ("swapoff", libc::SYS_swapoff),
    ("reboot", libc::SYS_reboot),
    ("settimeofday", libc::SYS_settimeofday),
    ("clock_settime", libc::SYS_clock_settime),
    ("clock_adjtime", libc::SYS_clock_adjtime),
    ("adjtimex", libc::SYS_adjtimex),
    ("acct", libc::SYS_acct),
    ("quotactl", libc::SYS_quotactl),
    ("open_by_handle_at", libc::SYS_open_by_handle_at),
    ("name_to_handle_at", libc::SYS_name_to_handle_at),
    ("io_uring_setup", libc::SYS_io_uring_setup),
    ("io_uring_enter", libc::SYS_io_uring_enter),
    ("io_uring_register", libc::SYS_io_uring_register),
    ("syslog", libc::SYS_syslog),
    ("vhangup", libc::SYS_vhangup),
    #[cfg(target_arch = "x86_64")]
    ("iopl", libc::SYS_iopl),
    #[cfg(target_arch = "x86_64")]
    ("ioperm", libc::SYS_ioperm),
];

fn arch() -> Result<TargetArch> {
    if cfg!(target_arch = "x86_64") {
        Ok(TargetArch::x86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(TargetArch::aarch64)
    } else {
        Err(anyhow!("seccomp filter: unsupported architecture"))
    }
}

/// Compiled classic-BPF program, serialized the way `bwrap --seccomp FD` expects
/// (struct sock_filter[], native endian).
pub fn compile() -> Result<Vec<u8>> {
    let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = DENIED.iter().map(|(_, nr)| (*nr, vec![])).collect();
    let filter = SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::Errno(libc::EPERM as u32), arch()?)
        .map_err(|e| anyhow!("seccomp filter: {e}"))?;
    let prog: BpfProgram = filter.try_into().map_err(|e| anyhow!("seccomp compile: {e:?}"))?;
    let mut out = Vec::with_capacity(prog.len() * 8);
    for ins in prog {
        out.extend_from_slice(&ins.code.to_ne_bytes());
        out.push(ins.jt);
        out.push(ins.jf);
        out.extend_from_slice(&ins.k.to_ne_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn compiles() {
        let bpf = super::compile().unwrap();
        assert!(bpf.len() > 8 * super::DENIED.len());
        assert_eq!(bpf.len() % 8, 0);
    }
}
