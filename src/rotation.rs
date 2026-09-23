use crate::sources::ImageCandidate;
use crate::state::PersistedState;
use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

#[derive(Debug)]
pub struct RotationManager {
    pool: HashMap<String, ImageCandidate>,
    remaining: VecDeque<String>,
    shown_current_cycle: HashSet<String>,
    rng: SmallRng,
}

impl Default for RotationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RotationManager {
    pub fn new() -> Self {
        Self {
            pool: HashMap::new(),
            remaining: VecDeque::new(),
            shown_current_cycle: HashSet::new(),
            rng: SmallRng::from_rng(&mut rand::rng()),
        }
    }

    pub fn rebuild_pool(&mut self, candidates: Vec<ImageCandidate>) {
        self.pool = candidates
            .into_iter()
            .map(|candidate| (candidate.id.clone(), candidate))
            .collect();

        self.remaining.retain(|id| self.pool.contains_key(id));
        self.shown_current_cycle
            .retain(|id| self.pool.contains_key(id));

        let existing_remaining: HashSet<&str> = self.remaining.iter().map(|s| s.as_str()).collect();
        let mut new_ids: Vec<String> = self
            .pool
            .keys()
            .filter(|id| {
                !existing_remaining.contains(id.as_str())
                    && !self.shown_current_cycle.contains(id.as_str())
            })
            .cloned()
            .collect();
        new_ids.shuffle(&mut self.rng);
        self.remaining.extend(new_ids);

        if self.remaining.is_empty() {
            self.refill_cycle();
        }
    }

    pub fn restore_state(&mut self, state: &PersistedState) {
        self.remaining = state
            .remaining_queue
            .iter()
            .filter(|id| self.pool.contains_key(id.as_str()))
            .cloned()
            .collect();
        self.shown_current_cycle = state
            .shown_ids
            .iter()
            .filter(|id| self.pool.contains_key(id.as_str()))
            .cloned()
            .collect();

        if self.remaining.is_empty() {
            self.refill_cycle();
        }
    }

    pub fn export_state(&self) -> PersistedState {
        PersistedState {
            remaining_queue: self.remaining.iter().cloned().collect(),
            shown_ids: self.shown_current_cycle.iter().cloned().collect(),
            last_image_id: None,
            favorites: Vec::new(),
        }
    }

    pub fn next(&mut self) -> Option<ImageCandidate> {
        if self.remaining.is_empty() {
            self.refill_cycle();
        }
        let id = self.remaining.pop_front()?;
        self.shown_current_cycle.insert(id.clone());
        self.pool.get(&id).cloned()
    }

    pub fn peek_next(&mut self) -> Option<ImageCandidate> {
        if self.remaining.is_empty() {
            self.refill_cycle();
        }
        let id = self.remaining.front()?;
        self.pool.get(id).cloned()
    }

    pub fn pool_size(&self) -> usize {
        self.pool.len()
    }

    pub fn candidates(&self) -> Vec<ImageCandidate> {
        self.pool.values().cloned().collect()
    }

    /// Drop every pool entry whose resolved local path matches `target` (for
    /// example an image the user deleted from the picker). Returns true if
    /// anything was removed.
    pub fn remove_by_path(&mut self, target: &Path) -> bool {
        let target = target
            .canonicalize()
            .unwrap_or_else(|_| target.to_path_buf());

        let removed_ids: Vec<String> = self
            .pool
            .iter()
            .filter_map(|(id, candidate)| {
                match candidate.local_path() {
                    Ok(Some(path)) => {
                        let comparable = path.canonicalize().unwrap_or_else(|_| path);
                        if comparable == target {
                            Some(id.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            })
            .collect();

        let removed = !removed_ids.is_empty();
        for id in removed_ids {
            self.pool.remove(&id);
            self.remaining.retain(|other| other != &id);
            self.shown_current_cycle.retain(|other| other != &id);
        }
        if self.remaining.is_empty() {
            self.refill_cycle();
        }
        removed
    }

    fn refill_cycle(&mut self) {
        if self.pool.is_empty() {
            self.remaining.clear();
            return;
        }

        if self.shown_current_cycle.len() >= self.pool.len() {
            self.shown_current_cycle.clear();
        }

        let mut ids: Vec<String> = self
            .pool
            .keys()
            .filter(|id| !self.shown_current_cycle.contains(id.as_str()))
            .cloned()
            .collect();
        ids.shuffle(&mut self.rng);
        self.remaining = ids.into_iter().collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{ImageCandidate, Origin};
    use std::path::PathBuf;

    fn candidate(id: &str) -> ImageCandidate {
        ImageCandidate::local(
            id.to_string(),
            Origin::Directory,
            PathBuf::from(format!("{id}.jpg")),
            None,
        )
    }

    #[test]
    fn no_repeat_before_cycle_reset() {
        let mut rotation = RotationManager::new();
        rotation.rebuild_pool(vec![candidate("a"), candidate("b"), candidate("c")]);
        let mut seen = HashSet::new();

        for _ in 0..3 {
            let next = rotation.next().unwrap();
            assert!(seen.insert(next.id));
        }
    }

    #[test]
    fn remove_by_path_drops_matching_candidate() {
        use std::path::Path;

        let mut rotation = RotationManager::new();
        rotation.rebuild_pool(vec![candidate("a"), candidate("b"), candidate("c")]);
        assert_eq!(rotation.pool_size(), 3);

        assert!(rotation.remove_by_path(Path::new("b.jpg")));
        assert_eq!(rotation.pool_size(), 2);
        assert!(rotation.remove_by_path(Path::new("missing.jpg")) == false);
        assert_eq!(rotation.pool_size(), 2);
    }
}
