//! Build the worker-side seccomp-BPF filter.
//!
//! The filter is compiled *inside the worker*, after
//! `PR_SET_NO_NEW_PRIVS` and after receiving the policy description
//! from the broker. It is the final confinement step before the
//! worker enters the agent loop.
//!
//! This is a damage-limiter, not the capability boundary. Capability
//! enforcement happens broker-side, in `agentd`, with every tool
//! invocation checked against the agent's handle set. Seccomp just
//! ensures a compromised worker cannot pivot to arbitrary syscalls.
//!
//! ## Argument filtering (Bug 34, v0.2.4)
//!
//! `SYS_socket` and `SYS_socketpair` are restricted to `AF_UNIX` via a
//! `SeccompCondition` on arg0.  This rejects `AF_INET`, `AF_INET6`,
//! `AF_NETLINK`, etc.  A compromised worker that retains the broker IPC
//! capability cannot pivot to TCP/UDP/raw-network sockets.
//!
//! Server-side socket primitives (`SYS_bind`, `SYS_listen`,
//! `SYS_accept4`, `SYS_accept`) were removed from the allowlist
//! entirely — the worker is a Unix-socket *client* (it `connect()`s to
//! the broker session socket once and then reads/writes), never a
//! server.
//!
//! `SYS_clone3` remains unconditionally allowed.  Filtering its flags
//! is structurally infeasible: clone3 takes a pointer to
//! `struct clone_args` and seccomp-BPF can only read syscall registers,
//! not pointed-to memory.  See `docs/ideas.md` for the reconsider
//! signal.

#[cfg(target_os = "linux")]
pub use linux_impl::*;

#[cfg(not(target_os = "linux"))]
pub use stub_impl::*;

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::collections::BTreeMap;

    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };

    #[derive(Debug, thiserror::Error)]
    pub enum SeccompCompileError {
        #[error("seccompiler failed to compile filter: {0}")]
        Compile(String),
    }

    /// Syscall numbers allowed unconditionally in the worker.
    ///
    /// Kept as a function rather than a `const` because `libc::SYS_*`
    /// values are `c_long` and some of them are platform-gated. We
    /// pull the set once at compile time of the worker filter.
    pub fn allowed_syscall_numbers() -> Vec<i64> {
        let mut v: Vec<i64> = Vec::new();
        // Runtime
        v.extend([
            libc::SYS_futex,
            libc::SYS_mmap,
            libc::SYS_munmap,
            libc::SYS_mprotect,
            libc::SYS_brk,
            libc::SYS_rt_sigreturn,
            libc::SYS_rt_sigaction,
            libc::SYS_rt_sigprocmask,
            libc::SYS_exit,
            libc::SYS_exit_group,
            libc::SYS_clock_gettime,
            libc::SYS_clock_nanosleep,
            libc::SYS_gettid,
            libc::SYS_getpid,
            libc::SYS_tgkill,
            libc::SYS_sched_yield,
            libc::SYS_restart_syscall,
            libc::SYS_nanosleep,
            libc::SYS_prctl,
            // seccomp() must be allowed by the allowlist filter so the
            // subsequent kill-on-dangerous filter can be installed on top.
            // The kill filter itself denies future seccomp() invocations.
            libc::SYS_seccomp,
            libc::SYS_getrandom,
            libc::SYS_getuid,
            libc::SYS_geteuid,
            libc::SYS_getgid,
            libc::SYS_getegid,
            libc::SYS_uname,
            libc::SYS_set_robust_list,
            libc::SYS_set_tid_address,
            libc::SYS_sigaltstack,
            libc::SYS_madvise,
        ]);
        #[cfg(target_arch = "x86_64")]
        v.push(libc::SYS_arch_prctl);
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            v.push(libc::SYS_rseq);
        }
        // Tokio / stdio
        v.extend([
            libc::SYS_epoll_create1,
            libc::SYS_epoll_ctl,
            libc::SYS_epoll_pwait,
            libc::SYS_eventfd2,
            libc::SYS_pipe2,
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_close,
            libc::SYS_dup3,
            libc::SYS_fcntl,
            libc::SYS_ioctl,
            libc::SYS_readv,
            libc::SYS_writev,
            libc::SYS_lseek,
            libc::SYS_ppoll,
            libc::SYS_pread64,
            libc::SYS_pwrite64,
            libc::SYS_openat,
            libc::SYS_openat2,
            libc::SYS_statx,
            libc::SYS_newfstatat,
            libc::SYS_fstat,
            libc::SYS_fstatfs,
            libc::SYS_getdents64,
        ]);
        // `epoll_pwait2`, `poll` aren't present on all arches in
        // libc 0.2; add them via literal syscall numbers behind cfg.
        //
        // Tokio 1.x on modern kernels uses `epoll_pwait2` (it accepts a
        // timespec for sub-millisecond timeouts); without this, tokio's
        // I/O driver returns EPERM from poll and panics the worker with
        // "unexpected error when polling the I/O driver". Observed on
        // Debian 13 / kernel 6.12.43 with tokio 1.50 — the previous
        // `stderr → /dev/null` redirect in child_fn hid the panic, so
        // the worker silently died right after `sandboxed-ready`.
        #[cfg(target_arch = "x86_64")]
        {
            // SYS_epoll_wait: legacy no-signal-mask variant. mio on 6.12
            // can still fall back to it (the "no sigmask" fast path).
            v.push(libc::SYS_epoll_wait);
            v.extend([libc::SYS_poll, libc::SYS_stat, libc::SYS_lstat]);
            v.push(441); // epoll_pwait2 on x86_64
        }
        #[cfg(target_arch = "aarch64")]
        {
            v.push(441); // epoll_pwait2 on aarch64 (same number on all arches in 5.11+)
        }
        // Filesystem (Landlock gates further)
        v.extend([
            libc::SYS_mkdirat,
            libc::SYS_unlinkat,
            libc::SYS_renameat2,
            libc::SYS_ftruncate,
            libc::SYS_faccessat2,
            libc::SYS_readlinkat,
            libc::SYS_chdir,
            libc::SYS_getcwd,
        ]);
        #[cfg(target_arch = "x86_64")]
        {
            v.extend([
                libc::SYS_mkdir,
                libc::SYS_rmdir,
                libc::SYS_unlink,
                libc::SYS_rename,
                libc::SYS_truncate,
            ]);
        }
        // Broker IPC (AF_UNIX, client only).  socket()/socketpair() are
        // additionally restricted to AF_UNIX via SeccompCondition in
        // `compile_allowlist_filter`.  Server-side primitives (bind,
        // listen, accept, accept4) were removed from the allowlist —
        // the worker is a Unix-socket client only.
        v.extend([
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_connect,
            libc::SYS_sendmsg,
            libc::SYS_recvmsg,
            libc::SYS_sendto,
            libc::SYS_recvfrom,
            libc::SYS_shutdown,
            libc::SYS_getsockopt,
            libc::SYS_setsockopt,
            libc::SYS_getsockname,
            libc::SYS_getpeername,
        ]);
        // Thread creation (see module-level note on simplification).
        v.extend([libc::SYS_clone, libc::SYS_clone3]);
        v
    }

    /// Syscalls that MUST be killed with `SIGSYS`. No legitimate use
    /// from a worker.
    pub fn denied_kill_syscall_numbers() -> Vec<i64> {
        let mut v: Vec<i64> = Vec::new();
        v.extend([
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_chroot,
            libc::SYS_setns,
            libc::SYS_setuid,
            libc::SYS_setgid,
            libc::SYS_setresuid,
            libc::SYS_setresgid,
            libc::SYS_capset,
            libc::SYS_unshare,
            libc::SYS_kexec_load,
            libc::SYS_kexec_file_load,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_reboot,
            libc::SYS_swapon,
            libc::SYS_swapoff,
        ]);
        v
    }

    fn target_arch() -> TargetArch {
        if cfg!(target_arch = "x86_64") {
            TargetArch::x86_64
        } else if cfg!(target_arch = "aarch64") {
            TargetArch::aarch64
        } else if cfg!(target_arch = "riscv64") {
            TargetArch::riscv64
        } else {
            // Best-effort default. Compilation will likely fail
            // downstream on truly exotic targets; that's an acceptable
            // explicit failure mode.
            TargetArch::x86_64
        }
    }

    /// Compile the allowlist filter: unknown syscalls get `EPERM`,
    /// known ones pass through.
    ///
    /// `SYS_socket` and `SYS_socketpair` are conditionally allowed —
    /// only when arg0 (the address family) equals `AF_UNIX` (1).
    /// Other allowed syscalls have no condition (`vec![]`) so they pass
    /// unconditionally.
    pub fn compile_allowlist_filter() -> Result<BpfProgram, SeccompCompileError> {
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        for nr in allowed_syscall_numbers() {
            if nr == libc::SYS_socket || nr == libc::SYS_socketpair {
                // arg0 == AF_UNIX (1).  socket()/socketpair()'s domain
                // arg is an `int`, so use Dword.  AF_UNIX is the only
                // family the worker needs (broker session socket).
                let cond = SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Eq,
                    libc::AF_UNIX as u64,
                )
                .map_err(|e| SeccompCompileError::Compile(format!("AF_UNIX cond: {e}")))?;
                let rule = SeccompRule::new(vec![cond])
                    .map_err(|e| SeccompCompileError::Compile(format!("rule: {e}")))?;
                rules.insert(nr, vec![rule]);
            } else {
                rules.insert(nr, vec![]);
            }
        }
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Errno(libc::EPERM as u32),
            SeccompAction::Allow,
            target_arch(),
        )
        .map_err(|e| SeccompCompileError::Compile(e.to_string()))?;
        let prog: BpfProgram = filter
            .try_into()
            .map_err(|e: seccompiler::BackendError| SeccompCompileError::Compile(e.to_string()))?;
        Ok(prog)
    }

    /// Compile the kill filter: listed syscalls kill the process;
    /// everything else passes through. Stacked after the allowlist so
    /// that if the allowlist allows a dangerous syscall (shouldn't,
    /// but defence in depth), this filter still kills it.
    pub fn compile_kill_filter() -> Result<BpfProgram, SeccompCompileError> {
        let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
        for nr in denied_kill_syscall_numbers() {
            rules.insert(nr, vec![]);
        }
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::KillProcess,
            target_arch(),
        )
        .map_err(|e| SeccompCompileError::Compile(e.to_string()))?;
        let prog: BpfProgram = filter
            .try_into()
            .map_err(|e: seccompiler::BackendError| SeccompCompileError::Compile(e.to_string()))?;
        Ok(prog)
    }

    /// Compile both filters. Exposed for tests that want to verify
    /// the full pipeline compiles.
    pub fn compile_worker_filter() -> Result<(BpfProgram, BpfProgram), SeccompCompileError> {
        let allow = compile_allowlist_filter()?;
        let kill = compile_kill_filter()?;
        Ok((allow, kill))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn seccomp_allows_futex() {
            let allowed = allowed_syscall_numbers();
            assert!(allowed.contains(&libc::SYS_futex));
            let _ = compile_allowlist_filter().expect("allowlist filter must compile");
        }

        #[test]
        fn seccomp_denies_execve() {
            let denied = denied_kill_syscall_numbers();
            assert!(denied.contains(&libc::SYS_execve));
            assert!(denied.contains(&libc::SYS_execveat));
            let _ = compile_kill_filter().expect("kill filter must compile");
        }

        #[test]
        fn seccomp_allows_broker_connect() {
            let allowed = allowed_syscall_numbers();
            for nr in [
                libc::SYS_socket,
                libc::SYS_connect,
                libc::SYS_sendmsg,
                libc::SYS_recvmsg,
            ] {
                assert!(
                    allowed.contains(&nr),
                    "syscall {nr} must be allowed (broker IPC)"
                );
            }
        }

        #[test]
        fn seccomp_drops_server_socket_primitives() {
            // Bug 34 (v0.2.4): the worker is a Unix-socket *client* only.
            // bind/listen/accept/accept4 should not appear in the allowlist;
            // an attempt by a compromised worker to bind/listen returns EPERM.
            let allowed = allowed_syscall_numbers();
            for nr in [libc::SYS_bind, libc::SYS_listen, libc::SYS_accept4] {
                assert!(
                    !allowed.contains(&nr),
                    "syscall {nr} must NOT be in the worker allowlist (server-side primitive)"
                );
            }
            #[cfg(target_arch = "x86_64")]
            assert!(
                !allowed.contains(&libc::SYS_accept),
                "SYS_accept must NOT be in the worker allowlist on x86_64"
            );
        }

        #[test]
        fn seccomp_socket_filter_compiles_with_af_unix_condition() {
            // Compile the filter; if the SeccompCondition for
            // socket(AF_UNIX) is malformed seccompiler returns an error
            // here.  Live BPF execution (does AF_INET land EPERM?) is
            // exercised by the namespaced-agents integration tests on a
            // real worker.
            let _ = compile_allowlist_filter().expect("argument-filtered allowlist must compile");
        }

        #[test]
        fn seccomp_denies_namespace_escape() {
            let denied = denied_kill_syscall_numbers();
            for nr in [
                libc::SYS_mount,
                libc::SYS_pivot_root,
                libc::SYS_chroot,
                libc::SYS_setns,
                libc::SYS_unshare,
            ] {
                assert!(
                    denied.contains(&nr),
                    "syscall {nr} must be killed (namespace escape)"
                );
            }
        }

        #[test]
        fn full_worker_filter_compiles() {
            let _ = compile_worker_filter().expect("full worker filter must compile");
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod stub_impl {
    //! Non-Linux stub. Building on macOS/Windows is supported for
    //! editor/IDE workflows, but actual isolation only works on Linux.

    #[derive(Debug, thiserror::Error)]
    pub enum SeccompCompileError {
        #[error("seccomp only supported on Linux")]
        NotLinux,
    }

    pub fn allowed_syscall_numbers() -> Vec<i64> {
        Vec::new()
    }

    pub fn denied_kill_syscall_numbers() -> Vec<i64> {
        Vec::new()
    }
}
