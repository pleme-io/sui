//! Nix profile management — symlink-based generation chains.
//!
//! Nix profiles are symlink chains:
//! `/nix/var/nix/profiles/system` -> `system-42-link` -> `/nix/store/...`
//!
//! This module provides [`ProfileManager`] for creating, listing, switching,
//! and rolling back profile generations without shelling out to `nix-env`.

use std::path::{Path, PathBuf};

// ── Error type ────────────────────────────────────────────────

/// Errors that can occur during profile operations.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid profile symlink")]
    InvalidProfile,
    #[error("generation {0} not found")]
    GenerationNotFound(u32),
    #[error("no current generation")]
    NoCurrentGeneration,
    #[error("no previous generation to rollback to")]
    NoPreviousGeneration,
}

// ── Core types ────────────────────────────────────────────────

/// A single profile generation.
#[derive(Debug, Clone)]
pub struct Generation {
    /// Generation number (1-based, monotonically increasing).
    pub number: u32,
    /// The store path this generation points to.
    pub path: PathBuf,
    /// When this generation was created (from symlink metadata).
    pub created: Option<std::time::SystemTime>,
    /// Whether this is the currently active generation.
    pub current: bool,
}

/// Manages a Nix-style symlink profile (e.g., system, per-user).
///
/// The profile lives as a symlink at `{profile_dir}/{profile_name}` pointing
/// to `{profile_dir}/{profile_name}-{N}-link` where N is the generation
/// number. Each generation link in turn points to a store path.
pub struct ProfileManager {
    profile_dir: PathBuf,
    profile_name: String,
}

impl ProfileManager {
    /// Create a new profile manager.
    ///
    /// # Arguments
    /// - `profile_dir` — directory containing profile symlinks
    /// - `name` — profile name (e.g., `"system"`)
    pub fn new(profile_dir: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        Self {
            profile_dir: profile_dir.into(),
            profile_name: name.into(),
        }
    }

    /// System profile manager using default paths.
    #[must_use]
    pub fn system() -> Self {
        Self::new("/nix/var/nix/profiles", "system")
    }

    /// Path to the main profile symlink (e.g., `/nix/var/nix/profiles/system`).
    #[must_use]
    pub fn profile_path(&self) -> PathBuf {
        self.profile_dir.join(&self.profile_name)
    }

    /// Path to a generation link (e.g., `/nix/var/nix/profiles/system-42-link`).
    fn generation_link(&self, gen_num: u32) -> PathBuf {
        self.profile_dir
            .join(format!("{}-{}-link", self.profile_name, gen_num))
    }

    /// Get the current generation number by reading the profile symlink.
    ///
    /// Returns `Ok(None)` if the profile symlink does not exist yet.
    pub fn current_generation(&self) -> Result<Option<u32>, ProfileError> {
        let profile = self.profile_path();
        if !profile.exists() {
            return Ok(None);
        }
        let target = std::fs::read_link(&profile)?;
        // Parse "system-42-link" from the target filename.
        let filename = target
            .file_name()
            .and_then(|f| f.to_str())
            .ok_or(ProfileError::InvalidProfile)?;
        let number = parse_generation_number(filename, &self.profile_name)?;
        Ok(Some(number))
    }

    /// List all generations, sorted by number.
    pub fn list_generations(&self) -> Result<Vec<Generation>, ProfileError> {
        if !self.profile_dir.exists() {
            return Ok(Vec::new());
        }

        let mut generations = Vec::new();
        let current = self.current_generation()?;

        let prefix = format!("{}-", self.profile_name);
        let suffix = "-link";

        for entry in std::fs::read_dir(&self.profile_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if !name_str.starts_with(&prefix) || !name_str.ends_with(suffix) {
                continue;
            }

            // Also skip the `-tmp-link` placeholder used during atomic switches.
            if name_str.ends_with("-tmp-link") {
                continue;
            }

            let mid = &name_str[prefix.len()..name_str.len() - suffix.len()];
            if let Ok(num) = mid.parse::<u32>() {
                let path = std::fs::read_link(entry.path())?;
                let created = entry.metadata().ok().and_then(|m| m.created().ok());
                generations.push(Generation {
                    number: num,
                    path,
                    created,
                    current: current == Some(num),
                });
            }
        }

        generations.sort_by_key(|g| g.number);
        Ok(generations)
    }

    /// Set the profile to a new store path, creating a new generation.
    ///
    /// Returns the new generation number.
    pub fn set(&self, store_path: &Path) -> Result<u32, ProfileError> {
        std::fs::create_dir_all(&self.profile_dir)?;

        let next = self.next_generation_number()?;
        let link = self.generation_link(next);

        // Create the generation symlink: system-N-link -> /nix/store/...
        std::os::unix::fs::symlink(store_path, &link)?;

        // Atomically update the profile symlink: system -> system-N-link
        self.atomic_switch(&link)?;

        Ok(next)
    }

    /// Switch to a specific existing generation number.
    pub fn switch_generation(&self, gen_num: u32) -> Result<(), ProfileError> {
        let link = self.generation_link(gen_num);
        if !link.exists() {
            return Err(ProfileError::GenerationNotFound(gen_num));
        }

        self.atomic_switch(&link)?;
        Ok(())
    }

    /// Rollback to the previous generation.
    ///
    /// Returns the generation number that was switched to.
    pub fn rollback(&self) -> Result<u32, ProfileError> {
        let current = self
            .current_generation()?
            .ok_or(ProfileError::NoCurrentGeneration)?;

        // Find the highest generation less than current.
        let generations = self.list_generations()?;
        let prev = generations
            .iter()
            .filter(|g| g.number < current)
            .max_by_key(|g| g.number)
            .ok_or(ProfileError::NoPreviousGeneration)?;

        self.switch_generation(prev.number)?;
        Ok(prev.number)
    }

    /// Delete a specific generation (remove the `-N-link` symlink).
    ///
    /// Cannot delete the currently active generation.
    pub fn delete_generation(&self, gen_num: u32) -> Result<(), ProfileError> {
        let current = self.current_generation()?;
        if current == Some(gen_num) {
            return Err(ProfileError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cannot delete the current generation",
            )));
        }

        let link = self.generation_link(gen_num);
        if !link.exists() {
            return Err(ProfileError::GenerationNotFound(gen_num));
        }

        std::fs::remove_file(&link)?;
        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────

    /// Atomically replace the profile symlink via tmp + rename.
    fn atomic_switch(&self, target: &Path) -> Result<(), ProfileError> {
        let profile = self.profile_path();
        let tmp = self
            .profile_dir
            .join(format!("{}-tmp-link", self.profile_name));

        // Remove stale tmp if present from a previous crash.
        let _ = std::fs::remove_file(&tmp);

        std::os::unix::fs::symlink(target, &tmp)?;
        std::fs::rename(&tmp, &profile)?; // atomic on same filesystem
        Ok(())
    }

    fn next_generation_number(&self) -> Result<u32, ProfileError> {
        let generations = self.list_generations()?;
        Ok(generations.last().map(|g| g.number + 1).unwrap_or(1))
    }
}

/// Parse a generation number from a filename like `"system-42-link"`.
fn parse_generation_number(filename: &str, profile_name: &str) -> Result<u32, ProfileError> {
    let prefix = format!("{profile_name}-");
    let suffix = "-link";
    if !filename.starts_with(&prefix) || !filename.ends_with(suffix) {
        return Err(ProfileError::InvalidProfile);
    }
    let mid = &filename[prefix.len()..filename.len() - suffix.len()];
    mid.parse().map_err(|_| ProfileError::InvalidProfile)
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a fake store path inside a temp dir.
    fn fake_store_path(tmp: &Path, name: &str) -> PathBuf {
        let store = tmp.join("store");
        let p = store.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // ── set creates generation link and updates profile ──────

    #[test]
    fn set_creates_generation_and_profile_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "system");

        let store_path = fake_store_path(tmp.path(), "abc123-foo");
        let gen_num = pm.set(&store_path).unwrap();
        assert_eq!(gen_num, 1);

        // The profile symlink exists and ultimately resolves to the store path.
        let profile = pm.profile_path();
        assert!(profile.is_symlink());

        // The generation link exists.
        let gen_link = pm.generation_link(1);
        assert!(gen_link.is_symlink());
        let gen_target = std::fs::read_link(&gen_link).unwrap();
        assert_eq!(gen_target, store_path);
    }

    // ── current_generation reads the right number ────────────

    #[test]
    fn current_generation_returns_correct_number() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "system");

        let sp1 = fake_store_path(tmp.path(), "gen1-path");
        let sp2 = fake_store_path(tmp.path(), "gen2-path");

        pm.set(&sp1).unwrap();
        assert_eq!(pm.current_generation().unwrap(), Some(1));

        pm.set(&sp2).unwrap();
        assert_eq!(pm.current_generation().unwrap(), Some(2));
    }

    // ── list_generations finds all generations ────────────────

    #[test]
    fn list_generations_returns_all_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "test");

        let sp1 = fake_store_path(tmp.path(), "g1");
        let sp2 = fake_store_path(tmp.path(), "g2");
        let sp3 = fake_store_path(tmp.path(), "g3");

        pm.set(&sp1).unwrap();
        pm.set(&sp2).unwrap();
        pm.set(&sp3).unwrap();

        let generations = pm.list_generations().unwrap();
        assert_eq!(generations.len(), 3);
        assert_eq!(generations[0].number, 1);
        assert_eq!(generations[1].number, 2);
        assert_eq!(generations[2].number, 3);

        // Only the latest should be current.
        assert!(!generations[0].current);
        assert!(!generations[1].current);
        assert!(generations[2].current);
    }

    // ── switch_generation changes the profile target ─────────

    #[test]
    fn switch_generation_updates_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "myprofile");

        let sp1 = fake_store_path(tmp.path(), "first");
        let sp2 = fake_store_path(tmp.path(), "second");

        pm.set(&sp1).unwrap();
        pm.set(&sp2).unwrap();
        assert_eq!(pm.current_generation().unwrap(), Some(2));

        pm.switch_generation(1).unwrap();
        assert_eq!(pm.current_generation().unwrap(), Some(1));
    }

    // ── rollback goes to previous generation ─────────────────

    #[test]
    fn rollback_switches_to_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "sys");

        let sp1 = fake_store_path(tmp.path(), "a");
        let sp2 = fake_store_path(tmp.path(), "b");
        let sp3 = fake_store_path(tmp.path(), "c");

        pm.set(&sp1).unwrap();
        pm.set(&sp2).unwrap();
        pm.set(&sp3).unwrap();
        assert_eq!(pm.current_generation().unwrap(), Some(3));

        let prev = pm.rollback().unwrap();
        assert_eq!(prev, 2);
        assert_eq!(pm.current_generation().unwrap(), Some(2));
    }

    // ── multiple set calls increment generation numbers ──────

    #[test]
    fn multiple_sets_increment_generations() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "default");

        for i in 1..=5 {
            let sp = fake_store_path(tmp.path(), &format!("generation-{i}"));
            let number = pm.set(&sp).unwrap();
            assert_eq!(number, i);
        }
    }

    // ── generation with highest number but not current ───────

    #[test]
    fn highest_generation_not_current_after_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "test");

        let sp1 = fake_store_path(tmp.path(), "x1");
        let sp2 = fake_store_path(tmp.path(), "x2");
        let sp3 = fake_store_path(tmp.path(), "x3");

        pm.set(&sp1).unwrap();
        pm.set(&sp2).unwrap();
        pm.set(&sp3).unwrap();

        // Switch back to generation 1 — generation 3 is highest but not current.
        pm.switch_generation(1).unwrap();

        let generations = pm.list_generations().unwrap();
        assert_eq!(generations.len(), 3);
        assert!(generations[0].current);  // generation 1 is current
        assert!(!generations[1].current);
        assert!(!generations[2].current); // generation 3 is highest but not current
    }

    // ── empty profile directory ──────────────────────────────

    #[test]
    fn empty_profile_dir_returns_none_and_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("empty-profiles");
        // Do NOT create the directory.
        let pm = ProfileManager::new(&profiles_dir, "system");

        assert_eq!(pm.current_generation().unwrap(), None);
        assert!(pm.list_generations().unwrap().is_empty());
    }

    // ── set is atomic (tmp + rename pattern) ─────────────────

    #[test]
    fn set_does_not_leave_tmp_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "atomic");

        let sp = fake_store_path(tmp.path(), "store-path");
        pm.set(&sp).unwrap();

        // No tmp symlink should remain.
        let tmp_link = profiles_dir.join("atomic-tmp-link");
        assert!(!tmp_link.exists());
    }

    // ── rollback from generation 1 returns error ─────────────

    #[test]
    fn rollback_from_first_generation_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "sys");

        let sp = fake_store_path(tmp.path(), "only");
        pm.set(&sp).unwrap();

        let result = pm.rollback();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ProfileError::NoPreviousGeneration
        ));
    }

    // ── rollback with no current generation ──────────────────

    #[test]
    fn rollback_with_no_profile_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "sys");

        let result = pm.rollback();
        assert!(matches!(
            result.unwrap_err(),
            ProfileError::NoCurrentGeneration
        ));
    }

    // ── switch to non-existent generation ────────────────────

    #[test]
    fn switch_to_nonexistent_generation_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        let pm = ProfileManager::new(&profiles_dir, "test");

        let result = pm.switch_generation(99);
        assert!(matches!(
            result.unwrap_err(),
            ProfileError::GenerationNotFound(99)
        ));
    }

    // ── delete generation ────────────────────────────────────

    #[test]
    fn delete_generation_removes_link() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "test");

        let sp1 = fake_store_path(tmp.path(), "d1");
        let sp2 = fake_store_path(tmp.path(), "d2");

        pm.set(&sp1).unwrap();
        pm.set(&sp2).unwrap();

        pm.delete_generation(1).unwrap();
        let generations = pm.list_generations().unwrap();
        assert_eq!(generations.len(), 1);
        assert_eq!(generations[0].number, 2);
    }

    // ── cannot delete current generation ─────────────────────

    #[test]
    fn delete_current_generation_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "test");

        let sp = fake_store_path(tmp.path(), "current");
        pm.set(&sp).unwrap();

        let result = pm.delete_generation(1);
        assert!(result.is_err());
    }

    // ── generation paths point to correct store paths ────────

    #[test]
    fn generation_paths_are_correct() {
        let tmp = tempfile::tempdir().unwrap();
        let profiles_dir = tmp.path().join("profiles");
        let pm = ProfileManager::new(&profiles_dir, "test");

        let sp1 = fake_store_path(tmp.path(), "path-a");
        let sp2 = fake_store_path(tmp.path(), "path-b");

        pm.set(&sp1).unwrap();
        pm.set(&sp2).unwrap();

        let generations = pm.list_generations().unwrap();
        assert_eq!(generations[0].path, sp1);
        assert_eq!(generations[1].path, sp2);
    }

    // ── parse_generation_number ──────────────────────────────

    #[test]
    fn parse_gen_number_valid() {
        assert_eq!(parse_generation_number("system-42-link", "system").unwrap(), 42);
        assert_eq!(parse_generation_number("default-1-link", "default").unwrap(), 1);
    }

    #[test]
    fn parse_gen_number_invalid_format() {
        assert!(parse_generation_number("system-abc-link", "system").is_err());
        assert!(parse_generation_number("other-42-link", "system").is_err());
        assert!(parse_generation_number("system-42", "system").is_err());
        assert!(parse_generation_number("system--link", "system").is_err());
    }

    // ── system() constructor ─────────────────────────────────

    #[test]
    fn system_profile_has_expected_paths() {
        let pm = ProfileManager::system();
        assert_eq!(
            pm.profile_path(),
            PathBuf::from("/nix/var/nix/profiles/system")
        );
    }
}
