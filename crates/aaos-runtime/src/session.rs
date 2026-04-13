use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use aaos_core::{AgentId, Result, CoreError};
use aaos_llm::Message;

/// Trait for conversation session storage.
pub trait SessionStore: Send + Sync {
    /// Load all messages for an agent. Returns empty vec if no history exists.
    fn load(&self, agent_id: &AgentId) -> Result<Vec<Message>>;

    /// Append new messages to the agent's history.
    fn append(&self, agent_id: &AgentId, messages: &[Message]) -> Result<()>;

    /// Clear all history for an agent.
    fn clear(&self, agent_id: &AgentId) -> Result<()>;
}

/// JSONL-based session store. One file per agent: `{data_dir}/{agent_id}.jsonl`.
pub struct JsonlSessionStore {
    data_dir: PathBuf,
}

impl JsonlSessionStore {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data_dir).map_err(|e| {
            CoreError::Ipc(format!("failed to create session dir {}: {e}", data_dir.display()))
        })?;
        Ok(Self { data_dir })
    }

    fn path_for(&self, agent_id: &AgentId) -> PathBuf {
        self.data_dir.join(format!("{}.jsonl", agent_id.as_uuid()))
    }
}

impl SessionStore for JsonlSessionStore {
    fn load(&self, agent_id: &AgentId) -> Result<Vec<Message>> {
        let path = self.path_for(agent_id);
        if !path.exists() {
            return Ok(vec![]);
        }
        let file = fs::File::open(&path).map_err(|e| {
            CoreError::Ipc(format!("failed to open session file: {e}"))
        })?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(|e| CoreError::Ipc(format!("read error: {e}")))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: Message = serde_json::from_str(line)?;
            messages.push(msg);
        }
        Ok(messages)
    }

    fn append(&self, agent_id: &AgentId, messages: &[Message]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let path = self.path_for(agent_id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| CoreError::Ipc(format!("failed to open session file for append: {e}")))?;
        for msg in messages {
            let json = serde_json::to_string(msg)?;
            writeln!(file, "{json}").map_err(|e| {
                CoreError::Ipc(format!("failed to write session line: {e}"))
            })?;
        }
        Ok(())
    }

    fn clear(&self, agent_id: &AgentId) -> Result<()> {
        let path = self.path_for(agent_id);
        if path.exists() {
            fs::write(&path, b"").map_err(|e| {
                CoreError::Ipc(format!("failed to clear session file: {e}"))
            })?;
        }
        Ok(())
    }
}

/// In-memory session store for testing.
pub struct InMemorySessionStore {
    store: dashmap::DashMap<String, Vec<Message>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            store: dashmap::DashMap::new(),
        }
    }
}

impl SessionStore for InMemorySessionStore {
    fn load(&self, agent_id: &AgentId) -> Result<Vec<Message>> {
        Ok(self
            .store
            .get(&agent_id.as_uuid().to_string())
            .map(|v| v.clone())
            .unwrap_or_default())
    }

    fn append(&self, agent_id: &AgentId, messages: &[Message]) -> Result<()> {
        let key = agent_id.as_uuid().to_string();
        self.store
            .entry(key)
            .or_default()
            .extend(messages.iter().cloned());
        Ok(())
    }

    fn clear(&self, agent_id: &AgentId) -> Result<()> {
        let key = agent_id.as_uuid().to_string();
        self.store.remove(&key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaos_llm::ContentBlock;
    use tempfile::TempDir;

    #[test]
    fn jsonl_append_and_load() {
        let dir = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(dir.path().to_path_buf()).unwrap();
        let agent_id = AgentId::new();

        let messages = vec![
            Message::User { content: "hello".into() },
            Message::Assistant {
                content: vec![ContentBlock::Text { text: "hi there".into() }],
            },
        ];

        store.append(&agent_id, &messages).unwrap();
        let loaded = store.load(&agent_id).unwrap();
        assert_eq!(loaded.len(), 2);
        match &loaded[0] {
            Message::User { content } => assert_eq!(content, "hello"),
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn jsonl_multiple_appends_preserve_order() {
        let dir = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(dir.path().to_path_buf()).unwrap();
        let agent_id = AgentId::new();

        store.append(&agent_id, &[Message::User { content: "first".into() }]).unwrap();
        store.append(&agent_id, &[Message::User { content: "second".into() }]).unwrap();
        store.append(&agent_id, &[Message::User { content: "third".into() }]).unwrap();

        let loaded = store.load(&agent_id).unwrap();
        assert_eq!(loaded.len(), 3);
        match &loaded[2] {
            Message::User { content } => assert_eq!(content, "third"),
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn jsonl_load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(dir.path().to_path_buf()).unwrap();
        let loaded = store.load(&AgentId::new()).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn jsonl_clear_then_load() {
        let dir = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(dir.path().to_path_buf()).unwrap();
        let agent_id = AgentId::new();

        store.append(&agent_id, &[Message::User { content: "data".into() }]).unwrap();
        store.clear(&agent_id).unwrap();
        let loaded = store.load(&agent_id).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn jsonl_simulated_restart() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let agent_id = AgentId::new();

        {
            let store = JsonlSessionStore::new(path.clone()).unwrap();
            store.append(&agent_id, &[
                Message::User { content: "session 1 msg".into() },
            ]).unwrap();
        }

        {
            let store = JsonlSessionStore::new(path).unwrap();
            let loaded = store.load(&agent_id).unwrap();
            assert_eq!(loaded.len(), 1);
            match &loaded[0] {
                Message::User { content } => assert_eq!(content, "session 1 msg"),
                _ => panic!("expected User"),
            }
        }
    }

    #[test]
    fn in_memory_store_basic() {
        let store = InMemorySessionStore::new();
        let agent_id = AgentId::new();

        store.append(&agent_id, &[Message::User { content: "test".into() }]).unwrap();
        let loaded = store.load(&agent_id).unwrap();
        assert_eq!(loaded.len(), 1);

        store.clear(&agent_id).unwrap();
        let loaded = store.load(&agent_id).unwrap();
        assert!(loaded.is_empty());
    }
}
