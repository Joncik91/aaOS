use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent_id::AgentId;

/// A specific permission that can be granted to an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Capability {
    FileRead {
        path_glob: String,
    },
    FileWrite {
        path_glob: String,
    },
    WebSearch,
    NetworkAccess {
        hosts: Vec<String>,
    },
    SpawnChild {
        allowed_agents: Vec<String>,
    },
    ToolInvoke {
        tool_name: String,
    },
    MessageSend {
        target_agents: Vec<String>,
    },
    Custom {
        name: String,
        params: serde_json::Value,
    },
}

/// Rate limiting configuration for a capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    pub max_per_minute: u32,
}

/// Constraints that narrow a capability's scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_invocations: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

/// An unforgeable token granting a specific capability to a specific agent.
///
/// Tokens are created by the kernel at agent spawn time based on the
/// agent's manifest. They can only be narrowed (adding more constraints),
/// never escalated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityToken {
    pub id: Uuid,
    pub agent_id: AgentId,
    pub capability: Capability,
    pub constraints: Constraints,
    pub issued_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl CapabilityToken {
    /// Issue a new capability token for an agent.
    pub fn issue(agent_id: AgentId, capability: Capability, constraints: Constraints) -> Self {
        Self {
            id: Uuid::new_v4(),
            agent_id,
            capability,
            constraints,
            issued_at: Utc::now(),
            expires_at: None,
        }
    }

    /// Check if this token has expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Utc::now() > exp)
    }

    /// Check if this token grants the requested capability.
    pub fn permits(&self, requested: &Capability) -> bool {
        if self.is_expired() {
            return false;
        }
        self.capability_matches(requested)
    }

    fn capability_matches(&self, requested: &Capability) -> bool {
        match (&self.capability, requested) {
            (
                Capability::FileRead { path_glob: granted },
                Capability::FileRead { path_glob: req },
            ) => glob_matches(granted, req),
            (
                Capability::FileWrite { path_glob: granted },
                Capability::FileWrite { path_glob: req },
            ) => glob_matches(granted, req),
            (Capability::WebSearch, Capability::WebSearch) => true,
            (
                Capability::NetworkAccess { hosts: granted },
                Capability::NetworkAccess { hosts: req },
            ) => req.iter().all(|h| granted.contains(h)),
            (
                Capability::SpawnChild {
                    allowed_agents: granted,
                },
                Capability::SpawnChild {
                    allowed_agents: req,
                },
            ) => req.iter().all(|a| granted.contains(a)),
            (
                Capability::ToolInvoke { tool_name: granted },
                Capability::ToolInvoke { tool_name: req },
            ) => granted == req || granted == "*",
            (
                Capability::MessageSend {
                    target_agents: granted,
                },
                Capability::MessageSend { target_agents: req },
            ) => req
                .iter()
                .all(|a| granted.iter().any(|g| g == "*" || g == a)),
            (Capability::Custom { name: gn, .. }, Capability::Custom { name: rn, .. }) => gn == rn,
            _ => false,
        }
    }

    /// Create a narrowed copy of this token with additional constraints.
    pub fn narrow(&self, additional: Constraints) -> Self {
        let mut narrowed = self.clone();
        narrowed.id = Uuid::new_v4();
        if let Some(max) = additional.max_invocations {
            narrowed.constraints.max_invocations = Some(
                narrowed
                    .constraints
                    .max_invocations
                    .map_or(max, |existing| existing.min(max)),
            );
        }
        if let Some(rate) = additional.rate_limit {
            narrowed.constraints.rate_limit = Some(narrowed.constraints.rate_limit.map_or(
                rate.clone(),
                |existing| RateLimit {
                    max_per_minute: existing.max_per_minute.min(rate.max_per_minute),
                },
            ));
        }
        narrowed
    }
}

/// Simple glob matching: supports trailing `*` wildcards.
fn glob_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        path.starts_with(prefix)
    } else {
        pattern == path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> AgentId {
        AgentId::new()
    }

    #[test]
    fn file_read_glob_matching() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::FileRead {
                path_glob: "/data/project/*".into(),
            },
            Constraints::default(),
        );
        assert!(token.permits(&Capability::FileRead {
            path_glob: "/data/project/foo.txt".into()
        }));
        assert!(!token.permits(&Capability::FileRead {
            path_glob: "/etc/passwd".into()
        }));
    }

    #[test]
    fn tool_invoke_wildcard() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::ToolInvoke {
                tool_name: "*".into(),
            },
            Constraints::default(),
        );
        assert!(token.permits(&Capability::ToolInvoke {
            tool_name: "web_search".into()
        }));
    }

    #[test]
    fn capability_type_mismatch() {
        let token =
            CapabilityToken::issue(test_agent(), Capability::WebSearch, Constraints::default());
        assert!(!token.permits(&Capability::FileRead {
            path_glob: "/tmp/*".into()
        }));
    }

    #[test]
    fn narrowing_reduces_constraints() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::WebSearch,
            Constraints {
                max_invocations: Some(100),
                rate_limit: None,
            },
        );
        let narrowed = token.narrow(Constraints {
            max_invocations: Some(10),
            rate_limit: None,
        });
        assert_eq!(narrowed.constraints.max_invocations, Some(10));
    }

    #[test]
    fn token_roundtrips_json() {
        let token = CapabilityToken::issue(
            test_agent(),
            Capability::NetworkAccess {
                hosts: vec!["api.example.com".into()],
            },
            Constraints::default(),
        );
        let json = serde_json::to_string(&token).unwrap();
        let parsed: CapabilityToken = serde_json::from_str(&json).unwrap();
        assert_eq!(token.capability, parsed.capability);
    }
}
