use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use aaos_core::{AgentId, Result, CoreError};
use aaos_llm::Message;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A segment of archived conversation messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveSegment {
    pub source_range: (usize, usize),
    pub messages: Vec<Message>,
    pub archived_at: DateTime<Utc>,
}

/// Trait for conversation session storage.
pub trait SessionStore: Send + Sync {
    /// Load all messages for an agent. Returns empty vec if no history exists.
    fn load(&self, agent_id: &AgentId) -> Result<Vec<Message>>;

    /// Append new messages to the agent's history.
    fn append(&self, agent_id: &AgentId, messages: &[Message]) -> Result<()>;

    /// Clear all history for an agent.
    fn clear(&self, agent_id: &AgentId) -> Result<()>;

    /// Archive a segment of messages to durable storage.
    fn archive_segment(&self, agent_id: &AgentId, segment: &ArchiveSegment) -> Result<()>;

    /// Load all archive segments for an agent, sorted by archived_at ascending.
    /// Read-only — does NOT prune.
    fn load_archives(&self, agent_id: &AgentId) -> Result<Vec<ArchiveSegment>>;

    /// Delete archive segments older than max_age. Returns count of deleted archives.
    fn prune_archives(&self, agent_id: &AgentId, max_age: std::time::Duration) -> Result<usize>;
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

    fn archive_segment(&self, agent_id: &AgentId, segment: &ArchiveSegment) -> Result<()> {
        let filename = format!(
            "{}.archive.{}.json",
            agent_id.as_uuid(),
            uuid::Uuid::new_v4()
        );
        let path = self.data_dir.join(filename);
        let json = serde_json::to_string(segment)?;
        std::fs::write(&path, json).map_err(|e| {
            CoreError::Ipc(format!("failed to write archive file: {e}"))
        })?;
        Ok(())
    }

    fn load_archives(&self, agent_id: &AgentId) -> Result<Vec<ArchiveSegment>> {
        let prefix = format!("{}.archive.", agent_id.as_uuid());
        let mut archives = Vec::new();

        let entries = std::fs::read_dir(&self.data_dir).map_err(|e| {
            CoreError::Ipc(format!("failed to read session dir: {e}"))
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| CoreError::Ipc(format!("dir entry error: {e}")))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(".json") {
                let content = std::fs::read_to_string(entry.path()).map_err(|e| {
                    CoreError::Ipc(format!("failed to read archive file: {e}"))
                })?;
                let segment: ArchiveSegment = serde_json::from_str(&content)?;
                archives.push(segment);
            }
        }

        archives.sort_by_key(|a| a.archived_at);
        Ok(archives)
    }

    fn prune_archives(&self, agent_id: &AgentId, max_age: std::time::Duration) -> Result<usize> {
        let prefix = format!("{}.archive.", agent_id.as_uuid());
        let cutoff = Utc::now() - chrono::Duration::from_std(max_age)
            .unwrap_or_else(|_| chrono::Duration::days(365));
        let mut pruned = 0;

        let entries = std::fs::read_dir(&self.data_dir).map_err(|e| {
            CoreError::Ipc(format!("failed to read session dir: {e}"))
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| CoreError::Ipc(format!("dir entry error: {e}")))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(".json") {
                let content = std::fs::read_to_string(entry.path()).map_err(|e| {
                    CoreError::Ipc(format!("failed to read archive for pruning: {e}"))
                })?;
                if let Ok(segment) = serde_json::from_str::<ArchiveSegment>(&content) {
                    if segment.archived_at < cutoff {
                        std::fs::remove_file(entry.path()).map_err(|e| {
                            CoreError::Ipc(format!("failed to delete archive file: {e}"))
                        })?;
                        pruned += 1;
                    }
                }
            }
        }

        Ok(pruned)
    }
}

/// In-memory session store for testing.
pub struct InMemorySessionStore {
    store: dashmap::DashMap<String, Vec<Message>>,
    archives: dashmap::DashMap<String, Vec<ArchiveSegment>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            store: dashmap::DashMap::new(),
            archives: dashmap::DashMap::new(),
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

    fn archive_segment(&self, agent_id: &AgentId, segment: &ArchiveSegment) -> Result<()> {
        let key = agent_id.as_uuid().to_string();
        self.archives.entry(key).or_default().push(segment.clone());
        Ok(())
    }

    fn load_archives(&self, agent_id: &AgentId) -> Result<Vec<ArchiveSegment>> {
        let key = agent_id.as_uuid().to_string();
        let mut archives = self.archives.get(&key)
            .map(|v| v.clone())
            .unwrap_or_default();
        archives.sort_by_key(|a| a.archived_at);
        Ok(archives)
    }

    fn prune_archives(&self, agent_id: &AgentId, max_age: std::time::Duration) -> Result<usize> {
        let key = agent_id.as_uuid().to_string();
        let cutoff = Utc::now() - chrono::Duration::from_std(max_age)
            .unwrap_or_else(|_| chrono::Duration::days(365));
        let mut pruned = 0;

        if let Some(mut entry) = self.archives.get_mut(&key) {
            let before = entry.len();
            entry.retain(|seg| seg.archived_at >= cutoff);
            pruned = before - entry.len();
        }

        Ok(pruned)
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

    #[test]
    fn jsonl_archive_and_load() {
        let dir = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(dir.path().to_path_buf()).unwrap();
        let agent_id = AgentId::new();

        let segment = ArchiveSegment {
            source_range: (0, 2),
            messages: vec![
                Message::User { content: "old message 1".into() },
                Message::User { content: "old message 2".into() },
            ],
            archived_at: chrono::Utc::now(),
        };

        store.archive_segment(&agent_id, &segment).unwrap();
        let archives = store.load_archives(&agent_id).unwrap();
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].messages.len(), 2);
        assert_eq!(archives[0].source_range, (0, 2));
    }

    #[test]
    fn jsonl_multiple_archives_sorted() {
        let dir = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(dir.path().to_path_buf()).unwrap();
        let agent_id = AgentId::new();

        let seg1 = ArchiveSegment {
            source_range: (0, 5),
            messages: vec![Message::User { content: "batch 1".into() }],
            archived_at: chrono::Utc::now() - chrono::Duration::hours(2),
        };
        let seg2 = ArchiveSegment {
            source_range: (6, 10),
            messages: vec![Message::User { content: "batch 2".into() }],
            archived_at: chrono::Utc::now(),
        };

        store.archive_segment(&agent_id, &seg1).unwrap();
        store.archive_segment(&agent_id, &seg2).unwrap();
        let archives = store.load_archives(&agent_id).unwrap();
        assert_eq!(archives.len(), 2);
        assert!(archives[0].archived_at <= archives[1].archived_at);
    }

    #[test]
    fn jsonl_prune_archives_by_age() {
        let dir = TempDir::new().unwrap();
        let store = JsonlSessionStore::new(dir.path().to_path_buf()).unwrap();
        let agent_id = AgentId::new();

        let old_seg = ArchiveSegment {
            source_range: (0, 5),
            messages: vec![Message::User { content: "old".into() }],
            archived_at: chrono::Utc::now() - chrono::Duration::days(60),
        };
        let new_seg = ArchiveSegment {
            source_range: (6, 10),
            messages: vec![Message::User { content: "new".into() }],
            archived_at: chrono::Utc::now(),
        };

        store.archive_segment(&agent_id, &old_seg).unwrap();
        store.archive_segment(&agent_id, &new_seg).unwrap();

        let pruned = store.prune_archives(&agent_id, std::time::Duration::from_secs(30 * 86400)).unwrap();
        assert_eq!(pruned, 1);

        let remaining = store.load_archives(&agent_id).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn in_memory_archive_basic() {
        let store = InMemorySessionStore::new();
        let agent_id = AgentId::new();

        let segment = ArchiveSegment {
            source_range: (0, 3),
            messages: vec![Message::User { content: "archived".into() }],
            archived_at: chrono::Utc::now(),
        };

        store.archive_segment(&agent_id, &segment).unwrap();
        let archives = store.load_archives(&agent_id).unwrap();
        assert_eq!(archives.len(), 1);
    }
}
