use crate::errors::Result;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub remaining_queue: Vec<String>,
    pub shown_ids: Vec<String>,
    pub last_image_id: Option<String>,
    #[serde(default)]
    pub favorites: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<PersistedState> {
        if !self.path.exists() {
            return Ok(PersistedState::default());
        }
        let data = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        let state: PersistedState = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse state {}", self.path.display()))?;
        Ok(state)
    }

    pub fn save(&self, state: &PersistedState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("tmp");
        let payload = serde_json::to_string_pretty(state)?;
        fs::write(&tmp, payload).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).with_context(|| {
            format!("failed to atomically replace state {}", self.path.display())
        })?;
        Ok(())
    }
}
