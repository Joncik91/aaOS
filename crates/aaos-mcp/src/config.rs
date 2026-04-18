use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub client: ClientConfig,
    #[serde(default)]
    pub server: ServerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientConfig {
    #[serde(default)]
    pub servers: Vec<ClientServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientServerConfig {
    /// Logical name; tools register as `mcp.<name>.<tool_name>`.
    pub name: String,
    pub transport: TransportKind,
    /// For `http` transport: base URL of the MCP server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// For `stdio` transport: command + args to spawn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_bind")]
    pub bind: String,
}

fn default_bind() -> String {
    "127.0.0.1:3781".into()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_bind(),
        }
    }
}

impl McpConfig {
    /// Load from `/etc/aaos/mcp-servers.yaml`. Returns `None` (both subsystems
    /// disabled) if the file is absent. Returns an error if present but unparseable.
    pub fn load() -> anyhow::Result<Option<Self>> {
        let path = std::path::Path::new("/etc/aaos/mcp-servers.yaml");
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)?;
        let config: McpConfig = serde_yaml::from_str(&text)?;
        Ok(Some(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let yaml = r#"
client:
  servers:
    - name: filesystem
      transport: http
      url: http://192.168.0.178:3002
    - name: git
      transport: stdio
      command: ["/usr/bin/mcp-git"]
server:
  enabled: true
  bind: "127.0.0.1:3781"
"#;
        let cfg: McpConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.client.servers.len(), 2);
        assert!(cfg.server.enabled);
        assert_eq!(cfg.server.bind, "127.0.0.1:3781");
    }

    #[test]
    fn empty_yaml_gives_defaults() {
        let cfg: McpConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.client.servers.is_empty());
        assert!(!cfg.server.enabled);
    }
}
