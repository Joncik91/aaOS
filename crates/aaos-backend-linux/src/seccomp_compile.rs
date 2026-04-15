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
//! ## Simplification for commit 2
//!
//! The plan calls for argument-filtering on `socket` (allow only
//! `AF_UNIX`) and `clone3` (allow only `CLONE_THREAD`). seccompiler 0.5
//! supports that via `SeccompCondition`, but getting the precise
//! register widths right is fiddly and out of scope for commit 2. We
//! simplify: `socket` is allowed unconditionally, `clone3` is allowed
//! unconditionally. Tightening these is a follow-up.

#[cfg(target_os = "linux")]
pub use linux_impl::*;

#[cfg(not(target_os = "linux"))]
pub use stub_impl::*;

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::collections::BTreeMap;

    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

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
            libc::SYS_futex, libc::SYS_mmap, libc::SYS_munmap,
            libc::SYS_mprotect, libc::SYS_brk,
            libc::SYS_rt_sigreturn, libc::SYS_rt_sigaction,
            libc::SYS_rt_sigprocmask,
            libc::SYS_exit, libc::SYS_exit_group,
            libc::SYS_clock_gettime, libc::SYS_clock_nanosleep,
            libc::SYS_gettid, libc::SYS_getpid, libc::SYS_tgkill,
            libc::SYS_sched_yield, libc::SYS_restart_syscall,
            libc::SYS_nanosleep, libc::SYS_prctl,
            libc::SYS_getrandom,
            libc::SYS_getuid, libc::SYS_geteuid,
            libc::SYS_getgid, libc::SYS_getegid,
            libc::SYS_uname,
            libc::SYS_set_robust_list, libc::SYS_set_tid_address,
            libc::SYS_sigaltstack, libc::SYS_madvise,
        ]);
        #[cfg(target_arch = "x86_64")]
        v.push(libc::SYS_arch_prctl);
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            v.push(libc::SYS_rseq);
        }
        // Tokio / stdio
        v.extend([
            libc::SYS_epoll_create1, libc::SYS_epoll_ctl,
            libc::SYS_epoll_pwait, libc::SYS_eventfd2, libc::SYS_pipe2,
            libc::SYS_read, libc::SYS_write, libc::SYS_close,
            libc::SYS_dup3, libc::SYS_fcntl, libc::SYS_ioctl,
            libc::SYS_readv, libc::SYS_writev,
            libc::SYS_lseek, libc::SYS_ppoll,
            libc::SYS_pread64, libc::SYS_pwrite64,
            libc::SYS_openat, libc::SYS_statx, libc::SYS_newfstatat,
            libc::SYS_fstat, libc::SYS_fstatfs,
            libc::SYS_getdents64,
        ]);
        // `epoll_pwait2`, `poll` aren't present on all arches in
        // libc 0.2; try via libc::SYS_epoll_pwait2 behind cfg.
        #[cfg(target_arch = "x86_64")]
        {
            v.extend([
                libc::SYS_poll,
                libc::SYS_stat,
                libc::SYS_lstat,
            ]);
        }
        // Filesystem (Landlock gates further)
        v.extend([
            libc::SYS_mkdirat, libc::SYS_unlinkat,
            libc::SYS_renameat2, libc::SYS_ftruncate,
            libc::SYS_faccessat2, libc::SYS_readlinkat,
            libc::SYS_chdir, libc::SYS_getcwd,
        ]);
        #[cfg(target_arch = "x86_64")]
        {
            v.extend([
                libc::SYS_mkdir, libc::SYS_rmdir,
                libc::SYS_unlink, libc::SYS_rename,
                libc::SYS_truncate,
            ]);
        }
        // Broker IPC (AF_UNIX); see module-level doc on the
        // argument-filtering simplification.
        v.extend([
            libc::SYS_socket, libc::SYS_socketpair, libc::SYS_connect,
            libc::SYS_accept4, libc::SYS_sendmsg, libc::SYS_recvmsg,
            libc::SYS_sendto, libc::SYS_recvfrom,
            libc::SYS_shutdown,
            libc::SYS_getsockopt, libc::SYS_setsockopt,
            libc::SYS_getsockname, libc::SYS_getpeername,
            libc::SYS_bind, libc::SYS_listen,
        ]);
        #[cfg(target_arch = "x86_64")]
        {
            v.push(libc::SYS_accept);
        }
        // Thread creation (see module-level note on simplification).
        v.extend([libc::SYS_clone, libc::SYS_clone3]);
        v
    }

    /// Syscalls that MUST be killed with `SIGSYS`. No legitimate use
    /// from a worker.
    pub fn denied_kill_syscall_numbers() -> Vec<i64> {
        let mut v: Vec<i64> = Vec::new();
        v.extend([
            libc::SYS_execve, libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv, libc::SYS_process_vm_writev,
            libc::SYS_mount, libc::SYS_umount2,
            libc::SYS_pivot_root, libc::SYS_chroot, libc::SYS_setns,
            libc::SYS_setuid, libc::SYS_setgid,
            libc::SYS_setresuid, libc::SYS_setresgid,
            libc::SYS_capset, libc::SYS_unshare,
            libc::SYS_kexec_load, libc::SYS_kexec_file_load,
            libc::SYS_init_module, libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_bpf, libc::SYS_perf_event_open,
            libc::SYS_reboot, libc::SYS_swapon, libc::SYS_swapoff,
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
    pub fn compile_allowlist_filter() -> Result<BpfProgram, SeccompCompileError> {
        let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
        for nr in allowed_syscall_numbers() {
            rules.insert(nr, vec![]);
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
    pub fn compile_worker_filter()
        -> Result<(BpfProgram, BpfProgram), SeccompCompileError>
    {
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
            let _ = compile_allowlist_filter()
                .expect("allowlist filter must compile");
        }

        #[test]
        fn seccomp_denies_execve() {
            let denied = denied_kill_syscall_numbers();
            assert!(denied.contains(&libc::SYS_execve));
            assert!(denied.contains(&libc::SYS_execveat));
            let _ = compile_kill_filter()
                .expect("kill filter must compile");
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
            let _ = compile_worker_filter()
                .expect("full worker filter must compile");
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
