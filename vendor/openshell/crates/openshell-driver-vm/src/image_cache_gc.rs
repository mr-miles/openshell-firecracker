// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Removal of unused disks from the image cache (not in upstream).
//!
//! Upstream never deletes a prepared image: every driver build and every image
//! a sandbox ever used stays under `<state_dir>/images/` until removed by hand.
//! Every cache entry name carries the driver version (and, for bootstrap
//! images, the guest runtime), so each upgrade leaves a full set of disks that
//! no later build reads.
//!
//! A cache entry is a directory in the image cache root holding a
//! `rootfs.ext4`. A collection pass:
//!
//! - **keeps** every entry a sandbox's `image-identity` names, running or
//!   stopped, and the bootstrap entry named in [`CURRENT_BOOTSTRAP_FILE`];
//! - **removes** every other entry not built by this driver build (its name
//!   starts with none of the current prefixes): no sandbox this driver runs
//!   can use it;
//! - **removes** an entry of this build once its last use is at least
//!   `max_idle` ago. Last use is the modification time of its
//!   [`LAST_USED_FILE`], which the driver rewrites whenever a sandbox using the
//!   entry starts, stops or is deleted; an entry without one counts from its
//!   `rootfs.ext4`.
//!
//! Staging directories, `preparations/`, `overlay-templates/` and plain files
//! are never touched. An entry is first renamed into a [`TRASH_PREFIX`]
//! directory so a half-deleted entry never looks like a cache hit; a pass also
//! finishes any removal a crash interrupted.
//!
//! The caller must exclude concurrent image resolution for the whole pass: the
//! driver holds its image-use lock exclusively and the cross-process
//! `preparation.lock`.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Rewritten whenever a sandbox using the cache entry starts, stops or is deleted.
pub const LAST_USED_FILE: &str = "last-used";
/// In the cache root: the entry name of the bootstrap image last resolved.
pub const CURRENT_BOOTSTRAP_FILE: &str = "current-bootstrap";
/// Entries being removed are renamed to `<prefix><name>` first.
const TRASH_PREFIX: &str = ".gc-";
const ROOTFS_IMAGE: &str = "rootfs.ext4";
const SANDBOX_IMAGE_IDENTITY_FILE: &str = "image-identity";
const RESERVED_DIRS: [&str; 2] = ["preparations", "overlay-templates"];

/// The cache directory name for an image identity: everything but ASCII
/// alphanumerics, `-`, `_` and `.` becomes `-`.
pub fn sanitize_identity(identity: &str) -> String {
    identity
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

pub struct Policy<'a> {
    /// `<state_dir>/images`.
    pub cache_root: &'a Path,
    /// `<state_dir>/sandboxes`.
    pub sandboxes_root: &'a Path,
    /// Sanitized name prefixes of cache entries this driver build creates.
    pub current_prefixes: &'a [String],
    pub max_idle: Duration,
    pub now: SystemTime,
}

#[derive(Debug, Default)]
pub struct Removed {
    pub name: String,
    pub bytes: u64,
    /// Not built by this driver build, rather than idle.
    pub other_build: bool,
}

#[derive(Debug, Default)]
pub struct Report {
    pub removed: Vec<Removed>,
    pub kept: usize,
    /// Entries that could not be inspected or removed, with the error.
    pub failed: Vec<(String, String)>,
}

/// Run one collection pass. Fails without removing anything if the sandboxes'
/// image references cannot all be read.
pub fn collect(policy: &Policy<'_>) -> io::Result<Report> {
    let mut keep = referenced_entries(policy.sandboxes_root)?;
    if let Some(bootstrap) = read_current_bootstrap(policy.cache_root)? {
        keep.insert(bootstrap);
    }

    let entries = match fs::read_dir(policy.cache_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Report::default()),
        Err(error) => return Err(error),
    };
    let mut report = Report::default();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if name.starts_with(TRASH_PREFIX) {
            if let Err(error) = fs::remove_dir_all(&path) {
                report.failed.push((name, error.to_string()));
            }
            continue;
        }
        if RESERVED_DIRS.contains(&name.as_str()) || name.contains(".staging-") {
            continue;
        }
        match is_regular_file(&path.join(ROOTFS_IMAGE)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                report.failed.push((name, error.to_string()));
                continue;
            }
        }
        if keep.contains(&name) {
            report.kept += 1;
            continue;
        }
        let other_build = !policy
            .current_prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix.as_str()));
        if !other_build {
            match last_used(&path) {
                Ok(at) => {
                    let idle = policy.now.duration_since(at).unwrap_or(Duration::ZERO);
                    if idle < policy.max_idle {
                        report.kept += 1;
                        continue;
                    }
                }
                Err(error) => {
                    report.failed.push((name, error.to_string()));
                    continue;
                }
            }
        }
        match remove_entry(policy.cache_root, &name) {
            Ok(bytes) => report.removed.push(Removed {
                name,
                bytes,
                other_build,
            }),
            Err(error) => report.failed.push((name, error.to_string())),
        }
    }
    Ok(report)
}

/// Record a use of the cache entry `name`. A missing entry is not an error.
pub fn mark_used(cache_root: &Path, name: &str, now: SystemTime) -> io::Result<()> {
    if !valid_entry_name(name) {
        return Ok(());
    }
    let directory = cache_root.join(name);
    if !directory.is_dir() {
        return Ok(());
    }
    let seconds = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    fs::write(directory.join(LAST_USED_FILE), format!("{seconds}\n"))
}

/// Record `name` as the bootstrap entry the driver currently resolves to.
pub fn record_current_bootstrap(cache_root: &Path, name: &str) -> io::Result<()> {
    let target = cache_root.join(CURRENT_BOOTSTRAP_FILE);
    let staged = cache_root.join(format!(".{CURRENT_BOOTSTRAP_FILE}.{}", std::process::id()));
    fs::write(&staged, format!("{name}\n"))?;
    fs::rename(&staged, &target)
}

pub fn read_current_bootstrap(cache_root: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(cache_root.join(CURRENT_BOOTSTRAP_FILE)) {
        Ok(raw) => Ok(Some(raw.trim().to_string()).filter(|name| valid_entry_name(name))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The cache entry name a sandbox state directory references, if any.
pub fn sandbox_entry(state_dir: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(state_dir.join(SANDBOX_IMAGE_IDENTITY_FILE)) {
        Ok(raw) => {
            let identity = raw.trim();
            Ok((!identity.is_empty()).then(|| sanitize_identity(identity)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn referenced_entries(sandboxes_root: &Path) -> io::Result<HashSet<String>> {
    let mut names = HashSet::new();
    let entries = match fs::read_dir(sandboxes_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(names),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = sandbox_entry(&entry.path())? {
            names.insert(name);
        }
    }
    Ok(names)
}

fn valid_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with(TRASH_PREFIX)
        && sanitize_identity(name) == name
}

fn is_regular_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn last_used(directory: &Path) -> io::Result<SystemTime> {
    match fs::symlink_metadata(directory.join(LAST_USED_FILE)) {
        Ok(metadata) => metadata.modified(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::symlink_metadata(directory.join(ROOTFS_IMAGE))?.modified()
        }
        Err(error) => Err(error),
    }
}

fn remove_entry(cache_root: &Path, name: &str) -> io::Result<u64> {
    let trash: PathBuf = cache_root.join(format!("{TRASH_PREFIX}{name}"));
    fs::rename(cache_root.join(name), &trash)?;
    let bytes = tree_bytes(&trash).unwrap_or(0);
    fs::remove_dir_all(&trash)?;
    Ok(bytes)
}

fn tree_bytes(path: &Path) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        total += tree_bytes(&entry?.path())?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const HOUR: Duration = Duration::from_secs(3600);
    const CURRENT_BOOT: &str = "sandbox-bootstrap-rootfs-ext4-v6-openshell-0.2.0-guest-aa-";
    const CURRENT_PREP: &str =
        "sandbox-prepared-rootfs-ext4-umoci-v6-openshell-0.2.0-image-account-";

    struct Fixture {
        root: PathBuf,
        prefixes: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "image-cache-gc-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("images")).unwrap();
            fs::create_dir_all(root.join("sandboxes")).unwrap();
            Self {
                root,
                prefixes: vec![CURRENT_BOOT.to_string(), CURRENT_PREP.to_string()],
            }
        }

        fn cache(&self) -> PathBuf {
            self.root.join("images")
        }

        fn image(&self, name: &str) -> PathBuf {
            let directory = self.cache().join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(ROOTFS_IMAGE), vec![0_u8; 4096]).unwrap();
            fs::write(directory.join("image-config.json"), b"{}").unwrap();
            directory
        }

        fn sandbox(&self, id: &str, identity: &str) {
            let directory = self.root.join("sandboxes").join(id);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join(SANDBOX_IMAGE_IDENTITY_FILE),
                format!("{identity}\n"),
            )
            .unwrap();
        }

        /// Run a pass `elapsed` after now (file times are "now").
        fn collect_after(&self, elapsed: Duration) -> Report {
            collect(&Policy {
                cache_root: &self.cache(),
                sandboxes_root: &self.root.join("sandboxes"),
                current_prefixes: &self.prefixes,
                max_idle: 48 * HOUR,
                now: SystemTime::now() + elapsed,
            })
            .unwrap()
        }

        fn exists(&self, name: &str) -> bool {
            self.cache().join(name).exists()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn removed_names(report: &Report) -> Vec<&str> {
        let mut names: Vec<&str> = report.removed.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn other_builds_go_at_once_unless_referenced() {
        let f = Fixture::new();
        let old_boot = "sandbox-bootstrap-rootfs-ext4-v6-openshell-0.1.0-guest-bb-1";
        let old_prep = "sandbox-prepared-rootfs-ext4-umoci-v5-openshell-0.1.0-image-account-1";
        let old_used = "sandbox-prepared-rootfs-ext4-umoci-v5-openshell-0.1.0-image-account-2";
        f.image(old_boot);
        f.image(old_prep);
        f.image(old_used);
        f.sandbox(
            "stopped",
            "sandbox-prepared-rootfs-ext4-umoci-v5:openshell-0.1.0:image-account:2",
        );
        let report = f.collect_after(Duration::ZERO);
        assert_eq!(removed_names(&report), vec![old_boot, old_prep]);
        assert!(
            report
                .removed
                .iter()
                .all(|r| r.other_build && r.bytes >= 4096)
        );
        assert!(f.exists(old_used));
    }

    #[test]
    fn a_version_that_extends_the_current_one_is_another_build() {
        let f = Fixture::new();
        let dev = "sandbox-prepared-rootfs-ext4-umoci-v6-openshell-0.2.0-dev.1-g1-image-account-1";
        f.image(dev);
        assert_eq!(removed_names(&f.collect_after(Duration::ZERO)), vec![dev]);
    }

    #[test]
    fn current_build_entries_go_after_max_idle() {
        let f = Fixture::new();
        let image = format!("{CURRENT_PREP}1");
        f.image(&image);
        assert!(f.collect_after(47 * HOUR).removed.is_empty());
        assert!(f.exists(&image));
        assert_eq!(
            removed_names(&f.collect_after(48 * HOUR)),
            vec![image.as_str()]
        );
        assert!(!f.exists(&image));
    }

    #[test]
    fn last_used_marker_restarts_the_clock() {
        let f = Fixture::new();
        let past = SystemTime::now() - 100 * HOUR;
        let built_long_ago = |name: &str| {
            let directory = f.image(name);
            fs::File::options()
                .write(true)
                .open(directory.join(ROOTFS_IMAGE))
                .unwrap()
                .set_modified(past)
                .unwrap();
        };
        let unmarked = format!("{CURRENT_PREP}1");
        let marked = format!("{CURRENT_PREP}2");
        built_long_ago(&unmarked);
        built_long_ago(&marked);
        mark_used(&f.cache(), &marked, SystemTime::now()).unwrap();

        let report = f.collect_after(Duration::ZERO);
        assert_eq!(removed_names(&report), vec![unmarked.as_str()]);
        assert!(!report.removed[0].other_build);
        assert!(f.exists(&marked));
    }

    #[test]
    fn referenced_and_current_bootstrap_never_expire() {
        let f = Fixture::new();
        let used = format!("{CURRENT_PREP}1");
        let boot = format!("{CURRENT_BOOT}1");
        let old_boot = format!("{CURRENT_BOOT}2");
        f.image(&used);
        f.image(&boot);
        f.image(&old_boot);
        f.sandbox("a", &used);
        record_current_bootstrap(&f.cache(), &boot).unwrap();
        let report = f.collect_after(1000 * HOUR);
        assert_eq!(removed_names(&report), vec![old_boot.as_str()]);
        assert_eq!(report.kept, 2);
    }

    #[test]
    fn non_entries_are_left_alone() {
        let f = Fixture::new();
        for name in ["preparations", "overlay-templates", "x.staging-1-2"] {
            f.image(name);
        }
        fs::create_dir_all(f.cache().join("no-rootfs")).unwrap();
        fs::write(f.cache().join("preparation.lock"), b"").unwrap();
        let report = f.collect_after(1000 * HOUR);
        assert!(report.removed.is_empty(), "{report:?}");
        for name in [
            "preparations",
            "overlay-templates",
            "x.staging-1-2",
            "no-rootfs",
        ] {
            assert!(f.exists(name), "{name}");
        }
    }

    #[test]
    fn interrupted_removals_are_finished() {
        let f = Fixture::new();
        f.image(".gc-leftover");
        f.collect_after(Duration::ZERO);
        assert!(!f.exists(".gc-leftover"));
    }

    #[test]
    fn unreadable_references_abort_the_pass() {
        let f = Fixture::new();
        let image = "sandbox-bootstrap-rootfs-ext4-v6-openshell-0.1.0-guest-bb-1";
        f.image(image);
        let state = f.root.join("sandboxes").join("broken");
        fs::create_dir_all(state.join(SANDBOX_IMAGE_IDENTITY_FILE)).unwrap();
        assert!(
            collect(&Policy {
                cache_root: &f.cache(),
                sandboxes_root: &f.root.join("sandboxes"),
                current_prefixes: &f.prefixes,
                max_idle: HOUR,
                now: SystemTime::now(),
            })
            .is_err()
        );
        assert!(f.exists(image));
    }

    #[test]
    fn markers_ignore_unsafe_names() {
        let f = Fixture::new();
        mark_used(&f.cache(), "../sandboxes", SystemTime::now()).unwrap();
        assert!(!f.root.join("sandboxes").join(LAST_USED_FILE).exists());
        fs::write(f.cache().join(CURRENT_BOOTSTRAP_FILE), "../x\n").unwrap();
        assert_eq!(read_current_bootstrap(&f.cache()).unwrap(), None);
    }

    #[test]
    fn sanitize_matches_the_cache_layout() {
        assert_eq!(
            sanitize_identity("sha256:abc/def@ghi"),
            "sha256-abc-def-ghi"
        );
    }
}
