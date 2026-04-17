# Secret isolation: API key off the ambient channels *(2026-04-17)*

Before Phase F-b puts aaOS on a cloud image where strangers actually install it, close the LLM-API-key exfiltration paths that the F-a install shape left open.

## The gap

Pre-commit state, inherited from Phase F-a:

1. **`/etc/default/aaos` was mode `0640 root:aaos`.** Any process running as any member of the `aaos` group could `cat` the file. That's every agent process, every operator who joined the group to submit goals, every `file_read` capability grant whose glob happened to include `/etc/default/aaos` (it shouldn't — but "shouldn't" isn't enforcement).
2. **`DEEPSEEK_API_KEY` stayed in the daemon's process environment.** Even after `AnthropicConfig::from_env()` / `OpenAiCompatConfig::deepseek_from_env()` copied it into an owned struct field, the env var was still readable via `/proc/<pid>/environ` and inherited by every child process the daemon spawned (`spawn_agent` tokio tasks share the env table; `NamespacedBackend` workers inherit via `execve`).

Both channels required root to exploit *on a hardened box* — but F-a doesn't promise a hardened box, and the `aaos` group is specifically documented as the right thing to join as an operator. "`/etc/default/aaos` is readable by every operator" was not the threat model.

## What shipped

Three changes in one commit, each closing a distinct channel:

### 1. Tighten `/etc/default/aaos` to `0600 root:root`

The systemd unit reads `EnvironmentFile=-/etc/default/aaos` **as root before dropping to `User=aaos`**, so the service still starts. Operators no longer read the file; agents in the `aaos` group no longer read the file; `file_read` capability grants whose globs incidentally match `/etc/default/*` no longer read the file.

`packaging/debian/postinst` now `chmod 0600 root:root`s the file on install and on upgrade. New installs should already land at the right mode (our `README` tells operators `chmod 600`), but existing installs with looser perms get tightened automatically.

### 2. Scrub the API key from the daemon's process environment

`agentd run` (and the bootstrap-mode path) now calls `scrub_api_key_env()` after the LLM client config has been built:

```rust
let server = if let Ok(config) = OpenAiCompatConfig::deepseek_from_env() {
    // key is now in config.api_key (owned struct field)
    scrub_api_key_env();
    // ... build server with config ...
}
```

The scrub does two things in order:

- **Zero the backing bytes.** `libc::getenv("DEEPSEEK_API_KEY")` returns a pointer into the stack region `execve` wrote the env at. Walk past `KEY=` and zero every byte until the NUL terminator. This is what `/proc/<pid>/environ` renders from — `std::env::remove_var` alone only unlinks libc's `environ[]` pointer array; the kernel's `mm->env_start..env_end` still points at the original bytes. Without the zeroing step, the key stays visible to anything that reads `/proc/<pid>/environ` even after `remove_var`.
- **Call `std::env::remove_var`.** Removes the entry from libc's `environ[]` so subsequent `std::env::var` calls in the daemon or its tokio tasks return `Err`, and `execve` of child processes inherits an env without the key.

Safe to run at startup because no tokio tasks or child processes exist yet — no concurrent `getenv` can race the zeroing. On Linux the `libc::getenv` path is used; on non-Linux hosts (for test/dev) the fallback is just `std::env::remove_var`.

### 3. Document the security contract in `man agentd`

The man page's `/etc/default/aaos` entry now says exactly what the mode must be and why — `0600 root:root`, systemd reads as root before dropping, the postinst enforces it on upgrade, and `agentd` additionally scrubs the env at startup so `/proc/<pid>/environ` doesn't leak the key either.

## Verified end-to-end

Fresh `debian:13` container. `.deb` installed clean. `/etc/default/aaos` at `0600 root:root`:

```
$ ls -l /etc/default/aaos
-rw------- 1 root root 53 Apr 17 07:33 /etc/default/aaos

$ runuser -u testop -- cat /etc/default/aaos
cat: /etc/default/aaos: Permission denied
```

`testop` is in the `aaos` group (can connect to the socket) but cannot read the env file. The daemon starts successfully (systemd-equivalent launch: root reads the env, then `runuser --preserve-environment -u aaos -- /usr/bin/agentd run`).

**Daemon's /proc/<pid>/environ AFTER scrub:**

```
$ tr '\0' '\n' < /proc/<agentd-pid>/environ | grep -i 'sk-\|key'
DEEPSEEK_API_KEY=
<key-value bytes scrubbed — no sk-... visible>

$ tr '\0' '\n' < /proc/<agentd-pid>/environ | grep -F 'sk-814bef0'
(no match)
```

The variable name stays (it's not secret) but the value bytes are zeroed. Direct search for the 40-character key prefix returns no match.

**Daemon still calls DeepSeek:**

```
$ agentd submit "fetch https://example.com and write a one-line summary to /data/secret-test.md"
[07:37:07] 2d2347e3    spawned fetcher
[07:37:07] 2d2347e3    tool: web_fetch {"url":"https://example.com"}
[07:37:07] 2d2347e3    tool: file_write {"content":"..."}
[07:37:12] 3112d452    tool: file_read {"path":"/var/lib/aaos/workspace/.../fetched_content.html"}
[07:37:24] 3112d452    tool: file_write {"content":"# Summary of example.com..."}
[07:37:26] bootstrap   complete (0k in / 0k out, 26s)
```

26 s, real content written to `/data/secret-test.md`. The LLM call path goes through `config.api_key` (owned struct field), not through `std::env::var`.

## What this doesn't close

- **Core dumps.** If `agentd` crashes and dumps core before the scrub runs, the key is in the dump. Not shipping coredumps on F-b (`/proc/sys/kernel/core_pattern = |/bin/false` via the image's sysctl defaults) closes this.
- **Post-startup `/etc/default/aaos` reads.** The daemon itself never re-reads the file; `EnvironmentFile=` is consumed once by systemd at startup. A changed key requires `systemctl restart agentd`.
- **Runtime memory read of `agentd`.** An attacker with `/proc/<pid>/mem` read access (needs `CAP_SYS_PTRACE` or matching UID) can still read `config.api_key` from heap. That's what `NamespacedBackend`'s "worker holds no handles at all" posture is for — but `agentd` itself remains the trust boundary. Fix is orthogonal: don't give anyone ptrace on `agentd`. F-b's image will harden via `ProtectKernelTunables=yes` and `SystemCallFilter=` in the service unit.
- **Logs.** We never log the key directly; still worth a grep-based CI check on staging logs before first F-b release.

## Cost

One commit (`<pending>`). `libc` added as a Linux-only dep on `agentd`. Three files touched (`main.rs`, `Cargo.toml`, `postinst`, `agentd.1.md`). No new tests — the verification is the real `/proc/<pid>/environ` check, not a unit test. End-to-end cost on DeepSeek ~26 s for the verification goal, well under $0.01.

## Takeaway

The thing that makes aaOS differentiated is the capability model, and the capability model is only as good as the secrets it protects. An API key on `0640 root:aaos` with an ambient `/proc/environ` copy is a hole the capability check can't plug — by the time the tool boundary fires, the key has already escaped. Close the ambient channels first; let the capability model do its real job on everything else.

A reminder for F-b: **the security pitch sells only if the obvious channels are already closed on the base install.** This is the shape of that pitch.
