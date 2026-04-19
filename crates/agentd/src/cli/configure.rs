//! `agentd configure` — interactive first-boot setup.
//!
//! Prompts for an LLM API key, writes `/etc/default/aaos` with mode 0600
//! root:root (or whatever the operator set on `--env-file`), and restarts
//! the daemon unless `--no-restart` was passed.
//!
//! This subcommand MUST run as root (or with write access to the env file
//! and systemctl). The early permission check fails fast with an operator-
//! friendly message instead of letting the file-write error up through.

use std::io::{self, BufRead, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;

const DEEPSEEK_VAR: &str = "DEEPSEEK_API_KEY";
const ANTHROPIC_VAR: &str = "ANTHROPIC_API_KEY";

pub async fn run(
    provider: String,
    key_from_env: Option<String>,
    env_file: PathBuf,
    no_restart: bool,
) -> anyhow::Result<()> {
    // Require root for the default env-file path.  Overridable env files
    // (e.g. a developer's ~/.aaos-dev) let non-root users test the flow.
    if env_file.as_path() == std::path::Path::new("/etc/default/aaos") && !is_root() {
        anyhow::bail!(
            "agentd configure writes /etc/default/aaos — run as root (e.g. `sudo agentd configure`) \
             or pass --env-file <path> for a non-root target."
        );
    }

    let var_name = match provider.as_str() {
        "deepseek" => DEEPSEEK_VAR,
        "anthropic" => ANTHROPIC_VAR,
        other => anyhow::bail!("unknown provider `{other}` — use `deepseek` or `anthropic`"),
    };

    let key = match key_from_env {
        Some(source_var) => read_key_from_env(&source_var)?,
        None => prompt_for_key(var_name)?,
    };

    validate_key_shape(&provider, &key)?;

    write_env_file(&env_file, var_name, &key)?;
    eprintln!(
        "✓ wrote {} ({}=***, mode 0600)",
        env_file.display(),
        var_name
    );

    if no_restart {
        eprintln!("(skipping restart — --no-restart set)");
    } else {
        restart_daemon()?;
    }

    Ok(())
}

fn is_root() -> bool {
    // SAFETY: geteuid is always safe and never fails.
    unsafe { libc::geteuid() == 0 }
}

fn read_key_from_env(var: &str) -> anyhow::Result<String> {
    std::env::var(var).map_err(|_| {
        anyhow::anyhow!(
            "--key-from-env {var} but that var is unset or empty; \
             export it or drop --key-from-env to prompt interactively."
        )
    })
}

fn prompt_for_key(var_name: &str) -> anyhow::Result<String> {
    // Not using rpassword — it's a dependency we don't want just for this,
    // and the operator is already root.  Echo visibly; they can clear
    // scrollback afterward.  A hint on terminal history is printed.
    eprintln!(
        "Paste the {var_name}.  The value echoes visibly; clear your terminal's \
         scrollback after if that's a concern."
    );
    eprint!("{var_name}: ");
    io::stderr().flush()?;

    let stdin = io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|e| anyhow::anyhow!("failed to read key from stdin: {e}"))?;
    let key = line.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("no key entered; aborting");
    }
    Ok(key)
}

fn validate_key_shape(provider: &str, key: &str) -> anyhow::Result<()> {
    // Cheap sanity checks — catch obvious paste errors early.  Real key
    // validity is enforced by the daemon on first LLM call.
    if key.len() < 10 {
        anyhow::bail!("key looks too short ({} chars); check the paste", key.len());
    }
    let expected_prefix = match provider {
        "deepseek" => Some("sk-"),
        "anthropic" => Some("sk-ant-"),
        _ => None,
    };
    if let Some(prefix) = expected_prefix {
        if !key.starts_with(prefix) {
            eprintln!(
                "warn: key doesn't start with `{prefix}` — continuing anyway \
                 (provider may have changed the format)."
            );
        }
    }
    Ok(())
}

fn write_env_file(env_file: &PathBuf, var_name: &str, key: &str) -> anyhow::Result<()> {
    // Atomic: write to a tempfile in the same directory, fsync, rename.
    // Mode 0600 set via OpenOptions so there's no window where the file
    // exists at a looser mode.  systemd reads EnvironmentFile= as root
    // before de-privileging to User=aaos, so root-only is correct.
    let parent = env_file.parent().ok_or_else(|| {
        anyhow::anyhow!("env_file has no parent directory: {}", env_file.display())
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", parent.display()))?;

    let tmp_path = env_file.with_extension("aaos-configure.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    let mut f = opts
        .open(&tmp_path)
        .map_err(|e| anyhow::anyhow!("open {}: {e}", tmp_path.display()))?;

    // Format: one KEY=value per line.  Comments + other keys from an
    // existing env file are preserved by rewriting from the template
    // below; we deliberately DO NOT merge — this subcommand's contract
    // is "seed the minimum".  Operators wanting more knobs copy from
    // /etc/default/aaos.example.
    let body = format!(
        "# Written by `agentd configure`.\n\
         # Do not commit or copy elsewhere.  Mode 0600 root:root by intent.\n\
         {var_name}={key}\n"
    );
    f.write_all(body.as_bytes())
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp_path.display()))?;
    f.sync_all()
        .map_err(|e| anyhow::anyhow!("fsync {}: {e}", tmp_path.display()))?;
    drop(f);

    // Explicit permission re-assert (the mode() above handles it, but an
    // operator tempering with an existing file deserves a re-assert).
    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| anyhow::anyhow!("chmod {}: {e}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, env_file).map_err(|e| {
        anyhow::anyhow!(
            "rename {} → {}: {e}",
            tmp_path.display(),
            env_file.display()
        )
    })?;

    Ok(())
}

fn restart_daemon() -> anyhow::Result<()> {
    // systemctl daemon-reload first — in case a prior .deb install added
    // the unit but systemd didn't pick it up yet — then restart.
    // Any failure here is operator-visible: we return Ok so the env file
    // write stays committed, but print what the operator should run.
    let reload = Command::new("systemctl").args(["daemon-reload"]).status();
    if !matches!(reload, Ok(s) if s.success()) {
        eprintln!(
            "warn: systemctl daemon-reload failed; run `sudo systemctl daemon-reload && sudo systemctl restart agentd` manually."
        );
        return Ok(());
    }

    let restart = Command::new("systemctl")
        .args(["restart", "agentd.service"])
        .status();
    match restart {
        Ok(s) if s.success() => {
            eprintln!("✓ systemctl restart agentd — daemon restarted");
        }
        _ => {
            eprintln!(
                "warn: systemctl restart agentd failed; run it manually, then check `systemctl status agentd` and `journalctl -u agentd`."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validate_accepts_deepseek_prefix() {
        assert!(validate_key_shape("deepseek", "sk-abcdefghij").is_ok());
    }

    #[test]
    fn validate_rejects_too_short_key() {
        let e = validate_key_shape("deepseek", "sk-x").unwrap_err();
        assert!(e.to_string().contains("too short"));
    }

    #[test]
    fn validate_warns_on_prefix_mismatch_but_passes() {
        // Warn-only — don't hard-fail on a paste that might be a real key
        // after a provider format change.
        assert!(validate_key_shape("anthropic", "sk-not-expected-yet").is_ok());
    }

    #[test]
    fn write_env_file_lands_mode_0600() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("aaos");
        write_env_file(&target, "DEEPSEEK_API_KEY", "aaaaaaaaaa").unwrap();
        let meta = std::fs::metadata(&target).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {:o}", mode);
        let body = std::fs::read_to_string(&target).unwrap();
        // Test value is the literal "aaaaaaaaaa" — low entropy, obviously
        // fake, below gitleaks' generic-api-key threshold.
        assert!(body.contains("DEEPSEEK_API_KEY=aaaaaaaaaa"));
    }

    #[test]
    fn write_env_file_overwrites_existing() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("aaos");
        std::fs::write(&target, "stale=garbage\n").unwrap();
        write_env_file(&target, "DEEPSEEK_API_KEY", "bbbbbbbbbbbbbbbb").unwrap();
        let body = std::fs::read_to_string(&target).unwrap();
        assert!(!body.contains("stale=garbage"));
        assert!(body.contains("bbbbbbbbbbbbbbbb"));
    }
}
