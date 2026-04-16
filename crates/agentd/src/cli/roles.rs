//! `agentd roles list|show|validate` — inspect the role catalog.

use std::path::PathBuf;

use aaos_runtime::plan::{Role, RoleCatalog};

pub async fn list(dir: PathBuf) -> anyhow::Result<()> {
    let cat = RoleCatalog::load_from_dir(&dir)
        .map_err(|e| anyhow::anyhow!("load {}: {e}", dir.display()))?;
    println!("{:<16} {:<20} {}", "NAME", "MODEL", "PARAMETERS");
    for name in cat.names() {
        let r = cat.get(name).unwrap();
        let params: Vec<String> = r
            .parameters
            .iter()
            .map(|(k, s)| {
                if s.required {
                    k.clone()
                } else {
                    format!("{k}?")
                }
            })
            .collect();
        println!("{:<16} {:<20} {}", name, r.model, params.join(", "));
    }
    Ok(())
}

pub async fn show(name: String, dir: PathBuf) -> anyhow::Result<()> {
    let cat = RoleCatalog::load_from_dir(&dir)
        .map_err(|e| anyhow::anyhow!("load {}: {e}", dir.display()))?;
    match cat.get(&name) {
        Some(r) => {
            let yaml = serde_yaml::to_string(r)?;
            println!("{yaml}");
            Ok(())
        }
        None => Err(anyhow::anyhow!("role not found: {name}")),
    }
}

pub async fn validate(path: PathBuf) -> anyhow::Result<()> {
    let body = std::fs::read_to_string(&path)?;
    let role: Role = serde_yaml::from_str(&body)
        .map_err(|e| anyhow::anyhow!("YAML parse: {e}"))?;
    if role.name.is_empty() {
        return Err(anyhow::anyhow!("role has empty name"));
    }
    if role.message_template.is_empty() {
        return Err(anyhow::anyhow!("role has empty message_template"));
    }
    println!("ok: role '{}' ({} params)", role.name, role.parameters.len());
    Ok(())
}
