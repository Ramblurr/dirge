//! Skill usage telemetry sidecar.
//!
//! Port of Hermes's `tools/skill_usage.py`. Tracks per-skill activity
//! counters, lifecycle state, and provenance in a `.usage.json`
//! sidecar file at `.dirge/skills/.usage.json`.
//!
//! Key design decisions from Hermes preserved:
//! - Sidecar, not frontmatter — keeps telemetry out of SKILL.md
//! - Atomic writes via tempfile + rename
//! - File locking for read-modify-write safety
//! - All counter bumps are best-effort — failures never break the
//!   underlying tool call
//! - Provenance filter: only agent-created skills are curator-managed

use std::collections::HashMap;
use std::path::PathBuf;

use crate::extras::dirge_paths::ProjectPaths;

/// Lifecycle state tracked by the curator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillState {
    Active,
    Stale,
    Archived,
}

/// Per-skill telemetry record. Port of Hermes's skill_usage.py record shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default)]
    pub use_count: u64,
    #[serde(default)]
    pub view_count: u64,
    #[serde(default)]
    pub patch_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_patched_at: Option<String>,
    pub created_at: String,
    #[serde(default = "default_state")]
    pub state: SkillState,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

fn default_state() -> SkillState {
    SkillState::Active
}

impl SkillUsage {
    fn new(created_by: Option<&str>) -> Self {
        SkillUsage {
            created_by: created_by.map(|s| s.to_string()),
            use_count: 0,
            view_count: 0,
            patch_count: 0,
            last_used_at: None,
            last_viewed_at: None,
            last_patched_at: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            state: SkillState::Active,
            pinned: false,
            archived_at: None,
        }
    }
}

/// Sidecar store for skill telemetry at `.dirge/skills/.usage.json`.
/// Thread-safe via internal locking — all methods take `&mut self`.
#[derive(Clone)]
pub struct UsageStore {
    path: PathBuf,
    lock_path: PathBuf,
    data: HashMap<String, SkillUsage>,
}

impl UsageStore {
    /// Load the usage sidecar, creating an empty store if the file
    /// doesn't exist. Corrupt JSON results in a fresh start (best-effort,
    /// never blocks skill operations).
    pub fn load(paths: &ProjectPaths) -> Result<Self, String> {
        let path = paths.skills_dir().join(".usage.json");
        let lock_path = paths.skills_dir().join(".usage.json.lock");

        let data = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                    tracing::debug!(
                        target: "dirge::skills::usage",
                        error = %e,
                        "Corrupt .usage.json — starting fresh"
                    );
                    HashMap::new()
                }),
                Err(e) => {
                    tracing::debug!(
                        target: "dirge::skills::usage",
                        error = %e,
                        "Cannot read .usage.json — starting fresh"
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };

        Ok(UsageStore {
            path,
            lock_path,
            data,
        })
    }

    /// Serialize and write atomically. Acquires an exclusive file lock
    /// before writing to prevent corruption from concurrent processes.
    /// Best-effort: failures are logged and silently dropped.
    pub fn save(&self) -> Result<(), String> {
        let _lock = acquire_usage_lock(&self.lock_path)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create usage directory: {e}"))?;
        }
        let content = serde_json::to_string_pretty(&self.data)
            .map_err(|e| format!("Failed to serialize usage: {e}"))?;
        crate::fs_atomic::atomic_write_sync(&self.path, content.as_bytes())
            .map_err(|e| format!("Failed to write usage: {e}"))
    }

    fn now_iso() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn get_or_create(&mut self, name: &str, created_by: Option<&str>) -> &mut SkillUsage {
        self.data
            .entry(name.to_string())
            .or_insert_with(|| SkillUsage::new(created_by))
    }

    /// Record a skill creation event.
    pub fn record_create(&mut self, name: &str, created_by: &str) {
        let entry = self
            .data
            .entry(name.to_string())
            .or_insert_with(|| SkillUsage::new(Some(created_by)));
        // If the entry already existed, don't overwrite created_by.
        if entry.created_by.is_none() {
            entry.created_by = Some(created_by.to_string());
        }
        if let Err(e) = self.save() {
            tracing::debug!(target: "dirge::skills::usage", error = %e, "record_create save failed");
        }
    }

    /// Record a skill use (agent invoked the skill).
    pub fn record_use(&mut self, name: &str) {
        let entry = self.get_or_create(name, None);
        entry.use_count = entry.use_count.saturating_add(1);
        entry.last_used_at = Some(Self::now_iso());
        if let Err(e) = self.save() {
            tracing::debug!(target: "dirge::skills::usage", error = %e, "record_use save failed");
        }
    }

    /// Record a skill view (read the skill content).
    pub fn record_view(&mut self, name: &str) {
        let entry = self.get_or_create(name, None);
        entry.view_count = entry.view_count.saturating_add(1);
        entry.last_viewed_at = Some(Self::now_iso());
        if let Err(e) = self.save() {
            tracing::debug!(target: "dirge::skills::usage", error = %e, "record_view save failed");
        }
    }

    /// Record a skill patch (content was modified).
    pub fn record_patch(&mut self, name: &str) {
        let entry = self.get_or_create(name, None);
        entry.patch_count = entry.patch_count.saturating_add(1);
        entry.last_patched_at = Some(Self::now_iso());
        if let Err(e) = self.save() {
            tracing::debug!(target: "dirge::skills::usage", error = %e, "record_patch save failed");
        }
    }

    /// Set the pinned flag. Pinned skills are exempt from curator transitions.
    #[allow(dead_code)]
    pub fn set_pinned(&mut self, name: &str, pinned: bool) -> Result<(), String> {
        let entry = self.get_or_create(name, None);
        entry.pinned = pinned;
        self.save()
    }

    /// Set the lifecycle state.
    pub fn set_state(&mut self, name: &str, state: SkillState) -> Result<(), String> {
        let entry = self.get_or_create(name, None);
        let is_archived = state == SkillState::Archived;
        entry.state = state;
        if is_archived {
            entry.archived_at = Some(Self::now_iso());
        }
        self.save()
    }

    /// Provenance filter: only skills created by the agent are
    /// curator-managed. Bundled/shipped skills have `created_by: None`
    /// or a non-"agent" value.
    pub fn is_agent_created(&self, name: &str) -> bool {
        self.data
            .get(name)
            .and_then(|u| u.created_by.as_deref())
            .map(|c| c == "agent")
            .unwrap_or(false)
    }

    /// Seconds since the most recent activity (max of last_used_at,
    /// last_patched_at). Returns None if the skill has never been used
    /// or patched (just created).
    pub fn activity_age_seconds(&self, name: &str) -> Option<u64> {
        let entry = self.data.get(name)?;
        let newest = [
            entry.last_used_at.as_deref(),
            entry.last_patched_at.as_deref(),
        ]
        .into_iter()
        .flatten()
        .max();
        let ts = newest?;
        let parsed = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
        let now = chrono::Utc::now();
        let age = now.signed_duration_since(parsed);
        Some(age.num_seconds().max(0) as u64)
    }

    /// Get a reference to the usage record for a skill, if it exists.
    pub fn get(&self, name: &str) -> Option<&SkillUsage> {
        self.data.get(name)
    }

    /// Iterate over all skill names tracked in the usage store.
    #[allow(dead_code)]
    pub fn skill_names(&self) -> impl Iterator<Item = &String> {
        self.data.keys()
    }
}

/// Acquire an exclusive file lock on the usage sidecar lock file.
/// Uses create-exclusive semantics with PID-based staleness detection
/// (same pattern as memory_store.rs).
fn acquire_usage_lock(lock_path: &PathBuf) -> Result<UsageLock, String> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create lock directory: {e}"))?;
    }
    for attempt in 0..50 {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut f) => {
                let pid = std::process::id().to_string();
                let _ = std::io::Write::write_all(&mut f, pid.as_bytes());
                return Ok(UsageLock {
                    path: lock_path.clone(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Check staleness on first attempt only.
                if attempt == 0 {
                    if let Ok(content) = std::fs::read_to_string(lock_path) {
                        let pid: Result<u32, _> = content.trim().parse();
                        if let Ok(pid) = pid {
                            // Check if process is still alive.
                            #[cfg(unix)]
                            {
                                unsafe {
                                    if libc::kill(pid as i32, 0) != 0 {
                                        let _ = std::fs::remove_file(lock_path);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                return Err(format!("Failed to acquire usage lock: {e}"));
            }
        }
    }
    Err("Timed out waiting for usage file lock".to_string())
}

struct UsageLock {
    path: PathBuf,
}

impl Drop for UsageLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> (ProjectPaths, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "dirge-usage-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ProjectPaths::new(&dir);
        (paths, dir)
    }

    #[test]
    fn load_empty_usage_store() {
        let (paths, _dir) = temp_project();
        let store = UsageStore::load(&paths).unwrap();
        assert!(store.data.is_empty());
    }

    #[test]
    fn record_create_sets_created_by() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_create("my-skill", "agent");
        assert_eq!(
            store.data.get("my-skill").unwrap().created_by.as_deref(),
            Some("agent")
        );
    }

    #[test]
    fn record_use_bumps_counter() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_use("my-skill");
        store.record_use("my-skill");
        assert_eq!(store.data.get("my-skill").unwrap().use_count, 2);
        assert!(store.data.get("my-skill").unwrap().last_used_at.is_some());
    }

    #[test]
    fn record_view_bumps_counter() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_view("my-skill");
        assert_eq!(store.data.get("my-skill").unwrap().view_count, 1);
    }

    #[test]
    fn record_patch_bumps_counter() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_patch("my-skill");
        store.record_patch("my-skill");
        assert_eq!(store.data.get("my-skill").unwrap().patch_count, 2);
    }

    #[test]
    fn is_agent_created_filters_correctly() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_create("agent-skill", "agent");
        store.record_create("bundled-skill", "bundled");

        assert!(store.is_agent_created("agent-skill"));
        assert!(!store.is_agent_created("bundled-skill"));
        assert!(!store.is_agent_created("nonexistent"));
    }

    #[test]
    fn null_created_by_is_not_agent_created() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        // Skills created via record_use without record_create get None created_by.
        store.record_use("unknown-origin");
        assert!(!store.is_agent_created("unknown-origin"));
    }

    #[test]
    fn set_pinned_and_state() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_create("my-skill", "agent");
        store.set_pinned("my-skill", true).unwrap();
        assert!(store.get("my-skill").unwrap().pinned);

        store.set_state("my-skill", SkillState::Archived).unwrap();
        assert_eq!(store.get("my-skill").unwrap().state, SkillState::Archived);
        assert!(store.get("my-skill").unwrap().archived_at.is_some());
    }

    #[test]
    fn activity_age_seconds_returns_correct_diff() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_use("my-skill");
        let age = store.activity_age_seconds("my-skill");
        assert!(age.is_some());
        assert!(age.unwrap() < 5, "activity age should be under 5 seconds");
    }

    #[test]
    fn roundtrip_save_and_reload() {
        let (paths, _dir) = temp_project();
        {
            let mut store = UsageStore::load(&paths).unwrap();
            store.record_create("test-skill", "agent");
            store.record_use("test-skill");
            store.record_patch("test-skill");
            // Explicit save.
            store.save().unwrap();
        }
        // Reload from disk.
        let store2 = UsageStore::load(&paths).unwrap();
        let entry = store2.get("test-skill").unwrap();
        assert_eq!(entry.created_by.as_deref(), Some("agent"));
        assert_eq!(entry.use_count, 1);
        assert_eq!(entry.patch_count, 1);
    }

    #[test]
    fn corrupt_json_recovers_gracefully() {
        let (paths, _dir) = temp_project();
        std::fs::create_dir_all(paths.skills_dir()).unwrap();
        std::fs::write(paths.skills_dir().join(".usage.json"), "not valid json{{{").unwrap();

        let store = UsageStore::load(&paths).unwrap();
        assert!(
            store.data.is_empty(),
            "corrupt JSON should result in empty store"
        );
    }
}
