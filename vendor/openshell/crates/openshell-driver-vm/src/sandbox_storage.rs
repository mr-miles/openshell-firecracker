// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-sandbox storage requests carried in a sandbox's `driver_config`
//! (not in upstream).
//!
//! A sandbox can ask for:
//!
//! - `overlay_disk_mib`: the size of its writable overlay disk, between
//!   [`MIN_OVERLAY_DISK_MIB`] and the operator's `max_overlay_disk_mib`.
//! - `files`: files and directories copied from the host into the guest.
//!   Every `source` is a relative path inside the operator's `files_root`, so
//!   a request can only name content the operator put there. `target` is an
//!   absolute guest path.
//!
//! Files reach the guest in two steps, without a new device:
//!
//! 1. Before the VM boots, the driver writes the selected files into a private
//!    staging directory in the overlay's upper layer
//!    (`/.openshell-provision/<generation>`), the same way it injects other
//!    guest configuration.
//! 2. A guest init drop-in, which runs as root before the supervisor starts,
//!    creates any missing parent directories of each target, copies the
//!    staged content into place with the requested owner and mode, and
//!    removes the staging directory.
//!
//! Entries are provisioned on the sandbox's first launch. An entry with
//! `refresh: true` is copied again on every start. If a previous launch
//! staged files that the guest never copied (the VM failed before its init
//! ran), every entry is staged again.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize as _;

use crate::lifecycle::GuestInitDropin;
use crate::rootfs::{ext4_image_has_directory, write_rootfs_image_tree};

/// Default for the largest overlay disk a sandbox may request (64 GiB). The
/// disk is a sparse file, so the host only spends what the guest writes.
pub const DEFAULT_MAX_OVERLAY_DISK_MIB: u64 = 65_536;
/// Smallest overlay disk a sandbox may request.
pub const MIN_OVERLAY_DISK_MIB: u64 = 256;
/// Default for the total bytes one sandbox may provision (256 MiB).
pub const DEFAULT_MAX_FILES_BYTES: u64 = 256 * 1024 * 1024;
/// Most `files` entries one sandbox may request.
pub const MAX_FILE_ENTRIES: usize = 64;
/// Most files and directories one request may expand to.
const MAX_TREE_ENTRIES: usize = 4096;

/// Guest directory the driver stages provisioned files in.
pub(crate) const FILES_STAGE_DIR: &str = "/.openshell-provision";
/// Name of the guest init drop-in that copies staged files into place.
pub(crate) const FILES_DROPIN_NAME: &str = "90-openshell-provision-files";

/// Guest paths a target may not be, or be inside: kernel filesystems, the
/// guest init's own files, and driver-owned state.
const RESERVED_TARGETS: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/tmp",
    "/opt/openshell",
    "/.openshell",
    "/.openshell-bootstrap",
    FILES_STAGE_DIR,
    "/srv/openshell-env.sh",
    "/srv/openshell-vm-sandbox-init.sh",
];

/// Operator limits for per-sandbox storage requests.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SandboxStorageConfig {
    /// Largest `overlay_disk_mib` a sandbox may request.
    pub max_overlay_disk_mib: u64,
    /// Host directory every `files[].source` is resolved in. Unset, requests
    /// with `files` are rejected.
    pub files_root: Option<PathBuf>,
    /// Total bytes one sandbox may provision.
    pub max_files_bytes: u64,
}

impl Default for SandboxStorageConfig {
    fn default() -> Self {
        Self {
            max_overlay_disk_mib: DEFAULT_MAX_OVERLAY_DISK_MIB,
            files_root: None,
            max_files_bytes: DEFAULT_MAX_FILES_BYTES,
        }
    }
}

impl SandboxStorageConfig {
    /// Validate operator settings against the driver's default overlay size.
    pub fn validate(&self, default_overlay_disk_mib: u64) -> Result<(), String> {
        if self.max_overlay_disk_mib < default_overlay_disk_mib {
            return Err(format!(
                "max overlay disk size {} MiB is smaller than the default overlay disk size {default_overlay_disk_mib} MiB",
                self.max_overlay_disk_mib
            ));
        }
        if let Some(root) = &self.files_root
            && !root.is_absolute()
        {
            return Err(format!(
                "files root '{}' must be an absolute path",
                root.display()
            ));
        }
        if self.max_files_bytes == 0 {
            return Err("max files bytes must be greater than zero".to_string());
        }
        Ok(())
    }
}

/// Who owns a provisioned file in the guest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FileOwner {
    /// The sandbox account the workload runs as.
    #[default]
    Sandbox,
    /// `root:root`.
    Root,
}

/// One `driver_config.files` entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileRequest {
    /// Path relative to the operator's files root: a file or a directory.
    pub source: String,
    /// Absolute guest path. A directory source is merged into it.
    pub target: String,
    /// Octal permission bits for a file source, such as `"0644"`.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub owner: FileOwner,
    /// Copy again on every start, not only on the first launch.
    #[serde(default)]
    pub refresh: bool,
}

/// Accept `overlay_disk_mib` as a whole number. `driver_config` arrives as a
/// protobuf `Struct`, whose numbers are all `f64`.
// The f64 is checked to be a whole number in the exactly representable range
// before it is converted.
#[allow(
    clippy::float_cmp,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) fn deserialize_optional_mib<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => {
            if let Some(value) = number.as_u64() {
                return Ok(Some(value));
            }
            match number.as_f64() {
                // 2^53: every integer up to it is exact in an f64.
                Some(value)
                    if value.fract() == 0.0 && (0.0..=9_007_199_254_740_992.0).contains(&value) =>
                {
                    Ok(Some(value as u64))
                }
                _ => Err(D::Error::custom(
                    "overlay_disk_mib must be a whole, non-negative number of MiB",
                )),
            }
        }
        Some(_) => Err(D::Error::custom("overlay_disk_mib must be a number")),
    }
}

/// Check a requested overlay size against the operator's limits.
pub(crate) fn validate_overlay_disk_mib(
    requested: Option<u64>,
    config: &SandboxStorageConfig,
) -> Result<(), String> {
    let Some(mib) = requested else {
        return Ok(());
    };
    if mib < MIN_OVERLAY_DISK_MIB || mib > config.max_overlay_disk_mib {
        return Err(format!(
            "overlay_disk_mib {mib} is outside the allowed range {MIN_OVERLAY_DISK_MIB} to {} MiB",
            config.max_overlay_disk_mib
        ));
    }
    Ok(())
}

/// Check `files` entries without touching the host filesystem.
pub(crate) fn validate_file_requests(
    files: &[FileRequest],
    config: &SandboxStorageConfig,
) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }
    if config.files_root.is_none() {
        return Err(
            "driver_config.files needs a host files root; the operator sets OPENSHELL_VM_FILES_ROOT"
                .to_string(),
        );
    }
    if files.len() > MAX_FILE_ENTRIES {
        return Err(format!(
            "driver_config.files has {} entries; at most {MAX_FILE_ENTRIES} are allowed",
            files.len()
        ));
    }
    let mut targets = HashSet::new();
    for (index, request) in files.iter().enumerate() {
        validate_source(&request.source)
            .map_err(|error| format!("files[{index}].source: {error}"))?;
        let target = normalize_target(&request.target)
            .map_err(|error| format!("files[{index}].target: {error}"))?;
        if !targets.insert(target.clone()) {
            return Err(format!("files[{index}].target '{target}' is listed twice"));
        }
        if let Some(mode) = &request.mode {
            parse_mode(mode).map_err(|error| format!("files[{index}].mode: {error}"))?;
        }
    }
    Ok(())
}

fn has_unsafe_chars(value: &str) -> bool {
    value.chars().any(char::is_control)
}

fn validate_source(source: &str) -> Result<(), String> {
    if source.is_empty() || has_unsafe_chars(source) {
        return Err("must be a non-empty path without control characters".to_string());
    }
    let path = Path::new(source);
    let mut normal = 0;
    for component in path.components() {
        match component {
            Component::Normal(_) => normal += 1,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "'{source}' must be a relative path inside the files root, without '..'"
                ));
            }
        }
    }
    if normal == 0 {
        return Err(format!(
            "'{source}' does not name anything inside the files root"
        ));
    }
    Ok(())
}

/// Normalize an absolute guest path (collapse `//` and trailing `/`) and
/// reject `/`, `.`/`..` components and reserved guest paths.
pub(crate) fn normalize_target(target: &str) -> Result<String, String> {
    if !target.starts_with('/') || has_unsafe_chars(target) {
        return Err(format!(
            "'{target}' must be an absolute guest path without control characters"
        ));
    }
    let mut normalized = String::new();
    for part in target.split('/').filter(|part| !part.is_empty()) {
        if part == "." || part == ".." {
            return Err(format!(
                "'{target}' must not contain '.' or '..' components"
            ));
        }
        normalized.push('/');
        normalized.push_str(part);
    }
    if normalized.is_empty() {
        return Err("the guest root '/' cannot be a target".to_string());
    }
    if let Some(reserved) = RESERVED_TARGETS
        .iter()
        .find(|reserved| path_is_within(&normalized, reserved))
    {
        return Err(format!(
            "'{normalized}' is inside reserved guest path '{reserved}'"
        ));
    }
    Ok(normalized)
}

/// Whether `path` is `base` or inside it. Both are normalized absolute paths.
fn path_is_within(path: &str, base: &str) -> bool {
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Parse octal permission bits. Set-user-ID and set-group-ID bits are refused:
/// a root-owned set-user-ID file would hand the workload root.
pub(crate) fn parse_mode(mode: &str) -> Result<u32, String> {
    let digits = mode.strip_prefix("0o").unwrap_or(mode);
    if digits.is_empty() || digits.len() > 4 {
        return Err(format!(
            "'{mode}' must be 1 to 4 octal digits, such as \"0644\""
        ));
    }
    let value = u32::from_str_radix(digits, 8)
        .map_err(|_| format!("'{mode}' must be octal digits, such as \"0644\""))?;
    if value & 0o6000 != 0 {
        return Err(format!("'{mode}' sets set-user-ID or set-group-ID bits"));
    }
    Ok(value)
}

/// Whether a source is a single file or a directory tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    File,
    Directory,
}

/// One file or directory inside a directory source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeEntry {
    /// `/`-separated path relative to the source directory.
    pub relative: String,
    /// Host file to copy; `None` for a directory.
    pub host_file: Option<PathBuf>,
    /// Guest permission bits: the host's, without set-ID bits.
    pub mode: u32,
}

/// A request resolved against the host files root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedFile {
    pub target: String,
    pub kind: FileKind,
    pub host_path: PathBuf,
    pub tree: Vec<TreeEntry>,
    pub owner: FileOwner,
    /// Guest permission bits of the target: the requested mode for a file,
    /// otherwise the host's without set-ID bits.
    pub mode: u32,
    pub refresh: bool,
}

/// Host permission bits that are carried into the guest.
fn guest_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o1777
}

/// Resolve every request inside the files root and enforce the size limits.
/// Symbolic links are refused, both as sources and inside source
/// directories, so nothing outside the root can be reached.
pub(crate) fn resolve_files(
    files: &[FileRequest],
    config: &SandboxStorageConfig,
) -> Result<Vec<ResolvedFile>, String> {
    validate_file_requests(files, config)?;
    let Some(root) = config.files_root.as_ref() else {
        return Ok(Vec::new());
    };
    let root = fs::canonicalize(root)
        .map_err(|error| format!("files root '{}': {error}", root.display()))?;
    let mut total_bytes = 0_u64;
    let mut tree_entries = 0_usize;
    let mut resolved = Vec::with_capacity(files.len());
    for (index, request) in files.iter().enumerate() {
        let target = normalize_target(&request.target)?;
        let candidate = root.join(&request.source);
        let link = fs::symlink_metadata(&candidate)
            .map_err(|error| format!("files[{index}].source '{}': {error}", request.source))?;
        if link.file_type().is_symlink() {
            return Err(format!(
                "files[{index}].source '{}' is a symbolic link; links are not provisioned",
                request.source
            ));
        }
        let host_path = fs::canonicalize(&candidate)
            .map_err(|error| format!("files[{index}].source '{}': {error}", request.source))?;
        if !host_path.starts_with(&root) {
            return Err(format!(
                "files[{index}].source '{}' resolves outside the files root",
                request.source
            ));
        }
        let requested_mode = request.mode.as_deref().map(parse_mode).transpose()?;
        let mut tree = Vec::new();
        let kind = if link.is_file() {
            total_bytes = total_bytes.saturating_add(link.len());
            FileKind::File
        } else if link.is_dir() {
            if request.mode.is_some() {
                return Err(format!(
                    "files[{index}].mode applies only to a file source; '{}' is a directory",
                    request.source
                ));
            }
            if let Some(reserved) = RESERVED_TARGETS
                .iter()
                .find(|reserved| path_is_within(reserved, &target))
            {
                return Err(format!(
                    "files[{index}].target '{target}' would merge a directory over reserved guest path '{reserved}'"
                ));
            }
            walk_source_dir(
                &host_path,
                "",
                &mut tree,
                &mut total_bytes,
                &mut tree_entries,
            )
            .map_err(|error| format!("files[{index}].source '{}': {error}", request.source))?;
            FileKind::Directory
        } else {
            return Err(format!(
                "files[{index}].source '{}' is not a regular file or directory",
                request.source
            ));
        };
        if total_bytes > config.max_files_bytes {
            return Err(format!(
                "driver_config.files totals more than the {} byte limit",
                config.max_files_bytes
            ));
        }
        resolved.push(ResolvedFile {
            target,
            kind,
            host_path,
            tree,
            owner: request.owner,
            mode: requested_mode.unwrap_or_else(|| guest_mode(&link)),
            refresh: request.refresh,
        });
    }
    Ok(resolved)
}

fn walk_source_dir(
    dir: &Path,
    relative: &str,
    tree: &mut Vec<TreeEntry>,
    total_bytes: &mut u64,
    tree_entries: &mut usize,
) -> Result<(), String> {
    let mut entries = fs::read_dir(dir)
        .map_err(|error| format!("read {}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {}: {error}", dir.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let name = name
            .to_str()
            .filter(|name| !has_unsafe_chars(name))
            .ok_or_else(|| {
                format!(
                    "{} has a name that is not plain UTF-8",
                    entry.path().display()
                )
            })?;
        let child = if relative.is_empty() {
            name.to_string()
        } else {
            format!("{relative}/{name}")
        };
        *tree_entries += 1;
        if *tree_entries > MAX_TREE_ENTRIES {
            return Err(format!(
                "more than {MAX_TREE_ENTRIES} files and directories"
            ));
        }
        // `DirEntry::metadata` does not follow symbolic links.
        let metadata = entry
            .metadata()
            .map_err(|error| format!("stat {}: {error}", entry.path().display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(format!(
                "'{child}' is a symbolic link; links are not provisioned"
            ));
        }
        if file_type.is_dir() {
            tree.push(TreeEntry {
                relative: child.clone(),
                host_file: None,
                mode: guest_mode(&metadata),
            });
            walk_source_dir(&entry.path(), &child, tree, total_bytes, tree_entries)?;
        } else if file_type.is_file() {
            *total_bytes = total_bytes.saturating_add(metadata.len());
            tree.push(TreeEntry {
                relative: child,
                host_file: Some(entry.path()),
                mode: guest_mode(&metadata),
            });
        } else {
            return Err(format!("'{child}' is not a regular file or directory"));
        }
    }
    Ok(())
}

/// Entries to copy on this launch.
pub(crate) fn due_files(
    resolved: &[ResolvedFile],
    first_launch: bool,
    previous_stage_pending: bool,
) -> Vec<&ResolvedFile> {
    resolved
        .iter()
        .filter(|file| first_launch || previous_stage_pending || file.refresh)
        .collect()
}

/// Staging layout under the generation directory: entry `i` is staged as
/// `<i>`, a file or a directory holding the source tree.
pub(crate) fn staging_entries(due: &[&ResolvedFile]) -> Vec<(String, Option<PathBuf>)> {
    let mut entries = Vec::new();
    for (index, file) in due.iter().enumerate() {
        match file.kind {
            FileKind::File => entries.push((index.to_string(), Some(file.host_path.clone()))),
            FileKind::Directory => {
                entries.push((index.to_string(), None));
                entries.extend(file.tree.iter().map(|entry| {
                    (
                        format!("{index}/{}", entry.relative),
                        entry.host_file.clone(),
                    )
                }));
            }
        }
    }
    entries
}

/// Quote `value` as one POSIX shell word.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The guest init drop-in that copies the staged entries into place.
///
/// Every file and directory is handled by its own command, so ownership and
/// permissions are set exactly: missing directories (including the target's
/// parents) are created owned by the entry's owner, directories that already
/// exist keep their owner and mode, and every copied file is chowned and
/// chmodded after the copy.
pub(crate) fn render_dropin(generation: &str, due: &[&ResolvedFile], uid: u32, gid: u32) -> String {
    let mut script = String::from(DROPIN_PRELUDE);
    writeln!(script, "stage=\"$stage_root/\"{}", shell_quote(generation))
        .expect("writing to String cannot fail");
    for (index, file) in due.iter().enumerate() {
        let owner = match file.owner {
            FileOwner::Sandbox => format!("{uid}:{gid}"),
            FileOwner::Root => "0:0".to_string(),
        };
        let target = shell_quote(&file.target);
        writeln!(script, "make_parents {target} {owner}").expect("writing to String cannot fail");
        match file.kind {
            FileKind::File => {
                write_provision_file(
                    &mut script,
                    &index.to_string(),
                    &file.target,
                    &owner,
                    file.mode,
                );
            }
            FileKind::Directory => {
                write_provision_dir(&mut script, &file.target, &owner, file.mode);
                for entry in &file.tree {
                    let guest_path = format!("{}/{}", file.target, entry.relative);
                    if entry.host_file.is_some() {
                        write_provision_file(
                            &mut script,
                            &format!("{index}/{}", entry.relative),
                            &guest_path,
                            &owner,
                            entry.mode,
                        );
                    } else {
                        write_provision_dir(&mut script, &guest_path, &owner, entry.mode);
                    }
                }
            }
        }
        writeln!(script, "echo \"openshell provision: \"{target}")
            .expect("writing to String cannot fail");
    }
    script.push_str("rm -rf \"$stage_root\"\n");
    script
}

fn write_provision_file(script: &mut String, staged: &str, target: &str, owner: &str, mode: u32) {
    writeln!(
        script,
        "provision_file {} {} {owner} {mode:04o}",
        shell_quote(staged),
        shell_quote(target)
    )
    .expect("writing to String cannot fail");
}

fn write_provision_dir(script: &mut String, target: &str, owner: &str, mode: u32) {
    writeln!(
        script,
        "provision_dir {} {owner} {mode:04o}",
        shell_quote(target)
    )
    .expect("writing to String cannot fail");
}

/// Shared part of the drop-in. `ROOT_PREFIX` is the guest root as seen by the
/// guest init (`/newroot`). Globbing is off so no path is ever expanded.
const DROPIN_PRELUDE: &str = r#"#!/bin/sh
# Generated by the OpenShell Firecracker VM driver: copy files provisioned
# from the host into place, then remove the staging directory.
set -euf
root="${ROOT_PREFIX:-}"
stage_root="$root/.openshell-provision"

fail() {
    echo "openshell provision: $*" >&2
    exit 1
}

# make_parents PATH OWNER: create every missing ancestor directory of guest
# path PATH, owned by OWNER with mode 0755. Existing directories are left as
# they are.
make_parents() {
    parent="${1%/*}"
    current=""
    old_ifs="$IFS"
    IFS=/
    for part in ${parent#/}; do
        current="$current/$part"
        if [ ! -e "$root$current" ] && [ ! -L "$root$current" ]; then
            mkdir "$root$current"
            chown "$2" "$root$current"
            chmod 0755 "$root$current"
        elif [ ! -d "$root$current" ]; then
            IFS="$old_ifs"
            fail "$current exists and is not a directory"
        fi
    done
    IFS="$old_ifs"
}

# provision_dir PATH OWNER MODE: make guest path PATH a directory. A directory
# this creates is owned by OWNER with MODE; an existing one is left as it is.
provision_dir() {
    dst="$root$1"
    if [ -L "$dst" ] || { [ -e "$dst" ] && [ ! -d "$dst" ]; }; then
        fail "$1 exists and is not a directory"
    fi
    if [ ! -e "$dst" ]; then
        mkdir "$dst"
        chown "$2" "$dst"
        chmod "$3" "$dst"
    fi
}

# provision_file STAGED PATH OWNER MODE: replace guest path PATH with staged
# file STAGED, owned by OWNER with MODE.
provision_file() {
    dst="$root$2"
    if [ -d "$dst" ] && [ ! -L "$dst" ]; then
        fail "$2 is a directory; the source is a file"
    fi
    rm -f "$dst"
    cp "$stage/$1" "$dst"
    chown "$3" "$dst"
    chmod "$4" "$dst"
}

"#;

/// Overlay upper-layer path of a guest path.
fn overlay_upper_path(guest_path: &str) -> String {
    format!("/upper/{}", guest_path.trim_start_matches('/'))
}

/// Resolve the requests, stage what is due on this launch into the overlay
/// disk, and return the drop-in that copies it into place. `None` when
/// nothing is due.
pub(crate) fn stage_files(
    overlay_disk: &Path,
    files: &[FileRequest],
    config: &SandboxStorageConfig,
    first_launch: bool,
    generation: &str,
    uid: u32,
    gid: u32,
) -> Result<Option<GuestInitDropin>, String> {
    if files.is_empty() {
        return Ok(None);
    }
    let resolved = resolve_files(files, config)?;
    let previous_stage_pending =
        ext4_image_has_directory(overlay_disk, &overlay_upper_path(FILES_STAGE_DIR))?;
    let due = due_files(&resolved, first_launch, previous_stage_pending);
    if due.is_empty() {
        return Ok(None);
    }
    let stage = overlay_upper_path(&format!("{FILES_STAGE_DIR}/{generation}"));
    write_rootfs_image_tree(overlay_disk, &stage, &staging_entries(&due))?;
    Ok(Some(GuestInitDropin::new(
        FILES_DROPIN_NAME,
        render_dropin(generation, &due, uid, gid),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_root(root: &Path) -> SandboxStorageConfig {
        SandboxStorageConfig {
            files_root: Some(root.to_path_buf()),
            ..SandboxStorageConfig::default()
        }
    }

    fn request(source: &str, target: &str) -> FileRequest {
        FileRequest {
            source: source.to_string(),
            target: target.to_string(),
            mode: None,
            owner: FileOwner::Sandbox,
            refresh: false,
        }
    }

    #[test]
    fn config_rejects_a_max_below_the_default_and_a_relative_root() {
        let config = SandboxStorageConfig::default();
        assert!(config.validate(4096).is_ok());
        assert!(config.validate(DEFAULT_MAX_OVERLAY_DISK_MIB + 1).is_err());
        let relative = SandboxStorageConfig {
            files_root: Some(PathBuf::from("relative/root")),
            ..SandboxStorageConfig::default()
        };
        assert!(relative.validate(4096).unwrap_err().contains("absolute"));
        let no_bytes = SandboxStorageConfig {
            max_files_bytes: 0,
            ..SandboxStorageConfig::default()
        };
        assert!(no_bytes.validate(4096).is_err());
    }

    #[test]
    fn overlay_size_must_be_within_the_operator_range() {
        let config = SandboxStorageConfig {
            max_overlay_disk_mib: 8192,
            ..SandboxStorageConfig::default()
        };
        assert!(validate_overlay_disk_mib(None, &config).is_ok());
        assert!(validate_overlay_disk_mib(Some(MIN_OVERLAY_DISK_MIB), &config).is_ok());
        assert!(validate_overlay_disk_mib(Some(8192), &config).is_ok());
        assert!(validate_overlay_disk_mib(Some(MIN_OVERLAY_DISK_MIB - 1), &config).is_err());
        let error = validate_overlay_disk_mib(Some(8193), &config).unwrap_err();
        assert!(error.contains("8192"), "{error}");
    }

    #[derive(serde::Deserialize)]
    struct Mib {
        #[serde(default, deserialize_with = "deserialize_optional_mib")]
        size: Option<u64>,
    }

    #[test]
    fn overlay_size_accepts_struct_numbers() {
        // protobuf Struct numbers reach the driver as f64.
        let parse = |json: &str| serde_json::from_str::<Mib>(json).map(|mib| mib.size);
        assert_eq!(parse(r#"{"size": 16384.0}"#).unwrap(), Some(16384));
        assert_eq!(parse(r#"{"size": 16384}"#).unwrap(), Some(16384));
        assert_eq!(parse("{}").unwrap(), None);
        assert!(parse(r#"{"size": 1.5}"#).is_err());
        assert!(parse(r#"{"size": -1}"#).is_err());
        assert!(parse(r#"{"size": "16384"}"#).is_err());
    }

    #[test]
    fn file_requests_parse_with_defaults_and_reject_unknown_keys() {
        let parsed: FileRequest =
            serde_json::from_str(r#"{"source": "a.txt", "target": "/etc/a.txt"}"#).unwrap();
        assert_eq!(parsed, request("a.txt", "/etc/a.txt"));
        let parsed: FileRequest = serde_json::from_str(
            r#"{"source": "a", "target": "/a", "mode": "0600", "owner": "root", "refresh": true}"#,
        )
        .unwrap();
        assert_eq!(parsed.owner, FileOwner::Root);
        assert!(parsed.refresh);
        assert!(
            serde_json::from_str::<FileRequest>(r#"{"source": "a", "target": "/a", "uid": 0}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<FileRequest>(
                r#"{"source": "a", "target": "/a", "owner": "bob"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn files_need_an_operator_root() {
        let error = validate_file_requests(&[request("a", "/a")], &SandboxStorageConfig::default())
            .unwrap_err();
        assert!(error.contains("OPENSHELL_VM_FILES_ROOT"), "{error}");
        assert!(validate_file_requests(&[], &SandboxStorageConfig::default()).is_ok());
    }

    #[test]
    fn sources_must_stay_relative_to_the_root() {
        let config = config_with_root(Path::new("/srv/files"));
        for source in ["", "/etc/passwd", "../outside", "a/../../b", ".", "a\nb"] {
            assert!(
                validate_file_requests(&[request(source, "/a")], &config).is_err(),
                "{source:?}"
            );
        }
        for source in ["a", "dir/a.txt", "./dir"] {
            assert!(
                validate_file_requests(&[request(source, "/a")], &config).is_ok(),
                "{source:?}"
            );
        }
    }

    #[test]
    fn targets_are_normalized_and_reserved_paths_refused() {
        assert_eq!(normalize_target("/etc//app/").unwrap(), "/etc/app");
        for target in [
            "relative",
            "/",
            "//",
            "/etc/../root",
            "/a/./b",
            "/proc/1",
            "/dev",
            "/opt/openshell/init.d/x",
            "/.openshell/state/x",
            "/.openshell-provision",
            "/srv/openshell-env.sh",
            "/a\tb",
        ] {
            assert!(normalize_target(target).is_err(), "{target:?}");
        }
        assert!(normalize_target("/opt/openshellish").is_ok());
        assert!(normalize_target("/srv/data").is_ok());
    }

    #[test]
    fn duplicate_targets_and_too_many_entries_are_rejected() {
        let config = config_with_root(Path::new("/srv/files"));
        let error = validate_file_requests(&[request("a", "/x/"), request("b", "/x")], &config)
            .unwrap_err();
        assert!(error.contains("twice"), "{error}");
        let many = vec![request("a", "/x"); MAX_FILE_ENTRIES + 1];
        assert!(validate_file_requests(&many, &config).is_err());
    }

    #[test]
    fn modes_are_octal_without_setid_bits() {
        assert_eq!(parse_mode("0644").unwrap(), 0o644);
        assert_eq!(parse_mode("755").unwrap(), 0o755);
        assert_eq!(parse_mode("0o600").unwrap(), 0o600);
        assert_eq!(parse_mode("1777").unwrap(), 0o1777);
        for mode in ["", "0899", "64a", "04755", "2755", "07777", "00644"] {
            assert!(parse_mode(mode).is_err(), "{mode:?}");
        }
    }

    #[test]
    fn resolution_stays_inside_the_root() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        fs::create_dir_all(root.join("conf/nested")).unwrap();
        fs::write(root.join("app.toml"), "x = 1\n").unwrap();
        fs::write(root.join("conf/a.txt"), "aaaa").unwrap();
        fs::write(root.join("conf/nested/b.txt"), "bb").unwrap();
        fs::write(base.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(base.path().join("secret"), root.join("escape")).unwrap();
        let config = config_with_root(&root);

        let mut file = request("app.toml", "/etc/app/app.toml");
        file.mode = Some("0600".to_string());
        let resolved = resolve_files(&[file, request("conf", "/sandbox/conf")], &config).unwrap();
        assert_eq!(resolved[0].kind, FileKind::File);
        assert_eq!(resolved[0].mode, 0o600);
        assert_eq!(resolved[1].kind, FileKind::Directory);
        let relatives: Vec<_> = resolved[1]
            .tree
            .iter()
            .map(|entry| (entry.relative.as_str(), entry.host_file.is_some()))
            .collect();
        assert_eq!(
            relatives,
            [("a.txt", true), ("nested", false), ("nested/b.txt", true)]
        );

        let error = resolve_files(&[request("escape", "/x")], &config).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(resolve_files(&[request("missing", "/x")], &config).is_err());

        std::os::unix::fs::symlink(base.path().join("secret"), root.join("conf/link")).unwrap();
        let error = resolve_files(&[request("conf", "/x")], &config).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[test]
    fn resolution_enforces_the_byte_limit_and_directory_rules() {
        let base = tempfile::tempdir().unwrap();
        fs::create_dir_all(base.path().join("dir")).unwrap();
        fs::write(base.path().join("big"), vec![0_u8; 2048]).unwrap();
        let config = SandboxStorageConfig {
            max_files_bytes: 1024,
            ..config_with_root(base.path())
        };
        let error = resolve_files(&[request("big", "/big")], &config).unwrap_err();
        assert!(error.contains("1024"), "{error}");

        let config = config_with_root(base.path());
        let mut with_mode = request("dir", "/data");
        with_mode.mode = Some("0700".to_string());
        assert!(
            resolve_files(&[with_mode], &config)
                .unwrap_err()
                .contains("mode")
        );
        // A directory merged into /opt could overwrite the guest init's files.
        let error = resolve_files(&[request("dir", "/opt")], &config).unwrap_err();
        assert!(error.contains("reserved"), "{error}");
    }

    fn resolved(target: &str, kind: FileKind, refresh: bool) -> ResolvedFile {
        ResolvedFile {
            target: target.to_string(),
            kind,
            host_path: PathBuf::from(format!("/host{target}")),
            tree: Vec::new(),
            owner: FileOwner::Sandbox,
            mode: 0o644,
            refresh,
        }
    }

    #[test]
    fn only_refresh_entries_are_due_after_the_first_launch() {
        let files = [
            resolved("/once", FileKind::File, false),
            resolved("/always", FileKind::File, true),
        ];
        assert_eq!(due_files(&files, true, false).len(), 2);
        let later = due_files(&files, false, false);
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].target, "/always");
        // A stage the guest never copied is redone in full.
        assert_eq!(due_files(&files, false, true).len(), 2);
    }

    #[test]
    fn staging_entries_put_each_source_under_its_index() {
        let mut dir = resolved("/data", FileKind::Directory, false);
        dir.tree = vec![
            TreeEntry {
                relative: "sub".to_string(),
                host_file: None,
                mode: 0o755,
            },
            TreeEntry {
                relative: "sub/f".to_string(),
                host_file: Some(PathBuf::from("/host/data/sub/f")),
                mode: 0o600,
            },
        ];
        let file = resolved("/etc/a", FileKind::File, false);
        let entries = staging_entries(&[&file, &dir]);
        assert_eq!(
            entries,
            [
                ("0".to_string(), Some(PathBuf::from("/host/etc/a"))),
                ("1".to_string(), None),
                ("1/sub".to_string(), None),
                (
                    "1/sub/f".to_string(),
                    Some(PathBuf::from("/host/data/sub/f"))
                ),
            ]
        );
    }

    #[test]
    fn the_dropin_quotes_paths_and_sets_owner_and_mode_per_entry() {
        let mut file = resolved("/etc/it's.conf", FileKind::File, false);
        file.mode = 0o640;
        file.owner = FileOwner::Root;
        let mut dir = resolved("/sandbox/data", FileKind::Directory, false);
        dir.mode = 0o750;
        dir.tree = vec![
            TreeEntry {
                relative: "sub".to_string(),
                host_file: None,
                mode: 0o700,
            },
            TreeEntry {
                relative: "sub/f".to_string(),
                host_file: Some(PathBuf::from("/host/sub/f")),
                mode: 0o600,
            },
        ];
        let script = render_dropin("abc123", &[&file, &dir], 1000, 1001);
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("set -euf\n"));
        assert!(script.contains("stage=\"$stage_root/\"'abc123'\n"));
        assert!(script.contains("make_parents '/etc/it'\\''s.conf' 0:0\n"));
        assert!(script.contains("provision_file '0' '/etc/it'\\''s.conf' 0:0 0640\n"));
        assert!(script.contains("make_parents '/sandbox/data' 1000:1001\n"));
        assert!(script.contains("provision_dir '/sandbox/data' 1000:1001 0750\n"));
        assert!(script.contains("provision_dir '/sandbox/data/sub' 1000:1001 0700\n"));
        assert!(script.contains("provision_file '1/sub/f' '/sandbox/data/sub/f' 1000:1001 0600\n"));
        assert!(script.ends_with("rm -rf \"$stage_root\"\n"));
    }

    /// Run the generated drop-in against a scratch guest root, as the current
    /// user (so owners are this user's ids), the way guest init does.
    #[test]
    fn the_dropin_copies_into_place_creating_parents_and_keeping_existing_dirs() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let guest = tempfile::tempdir().unwrap();
        let root = guest.path();
        let stage = root.join(".openshell-provision/gen1");
        fs::create_dir_all(stage.join("1/sub")).unwrap();
        fs::write(stage.join("0"), "config\n").unwrap();
        fs::write(stage.join("1/sub/f"), "data\n").unwrap();
        // An existing directory the merge must not re-own or re-mode.
        fs::create_dir_all(root.join("srv")).unwrap();
        fs::set_permissions(root.join("srv"), fs::Permissions::from_mode(0o711)).unwrap();
        // An existing file the copy replaces.
        fs::create_dir_all(root.join("etc/app")).unwrap();
        fs::write(root.join("etc/app/app.toml"), "old\n").unwrap();

        let me = fs::metadata(root).unwrap();
        let (uid, gid) = (me.uid(), me.gid());
        let mut file = resolved("/etc/app/app.toml", FileKind::File, false);
        file.mode = 0o600;
        let mut dir = resolved("/srv", FileKind::Directory, false);
        dir.tree = vec![
            TreeEntry {
                relative: "sub".to_string(),
                host_file: None,
                mode: 0o750,
            },
            TreeEntry {
                relative: "sub/f".to_string(),
                host_file: Some(PathBuf::from("/unused")),
                mode: 0o640,
            },
        ];
        let mut nested = resolved("/new/deep/file.txt", FileKind::File, false);
        nested.mode = 0o644;
        // Stage index 2 is the nested file; stage it too.
        fs::write(stage.join("2"), "nested\n").unwrap();
        let script = render_dropin("gen1", &[&file, &dir, &nested], uid, gid);
        let script_path = guest.path().join("dropin.sh");
        fs::write(&script_path, script).unwrap();

        let output = std::process::Command::new("sh")
            .arg(&script_path)
            .env("ROOT_PREFIX", root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let mode =
            |path: &str| fs::metadata(root.join(path)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            fs::read_to_string(root.join("etc/app/app.toml")).unwrap(),
            "config\n"
        );
        assert_eq!(mode("etc/app/app.toml"), 0o600);
        assert_eq!(mode("srv"), 0o711, "an existing directory keeps its mode");
        assert_eq!(mode("srv/sub"), 0o750);
        assert_eq!(
            fs::read_to_string(root.join("srv/sub/f")).unwrap(),
            "data\n"
        );
        assert_eq!(mode("srv/sub/f"), 0o640);
        assert_eq!(mode("new"), 0o755, "missing parents are created");
        assert_eq!(mode("new/deep"), 0o755);
        assert_eq!(
            fs::read_to_string(root.join("new/deep/file.txt")).unwrap(),
            "nested\n"
        );
        let owner = fs::metadata(root.join("new/deep")).unwrap();
        assert_eq!((owner.uid(), owner.gid()), (uid, gid));
        assert!(
            !root.join(".openshell-provision").exists(),
            "the stage is removed"
        );
    }

    #[test]
    fn stage_files_writes_due_entries_into_the_overlay() {
        if openshell_core::e2fsprogs::resolve(&["debugfs"]).is_err()
            || openshell_core::e2fsprogs::resolve(&["mke2fs", "mkfs.ext4"]).is_err()
        {
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("files");
        fs::create_dir_all(root.join("seed/sub")).unwrap();
        fs::write(root.join("app.toml"), "x = 1\n").unwrap();
        fs::write(root.join("seed/sub/data.txt"), "data\n").unwrap();
        let source = base.path().join("overlay-source");
        fs::create_dir_all(source.join("upper")).unwrap();
        fs::create_dir_all(source.join("work")).unwrap();
        let overlay = base.path().join("overlay.ext4");
        crate::rootfs::create_ext4_image_from_dir_with_size(&source, &overlay, 64 * 1024 * 1024)
            .unwrap();
        let config = config_with_root(&root);
        let mut refreshed = request("app.toml", "/etc/app/app.toml");
        refreshed.refresh = true;
        let files = [refreshed, request("seed", "/sandbox/seed")];

        let dropin = stage_files(&overlay, &files, &config, true, "gen1", 1000, 1000)
            .unwrap()
            .expect("the first launch stages every entry");
        assert_eq!(dropin.name, FILES_DROPIN_NAME);
        let script = String::from_utf8(dropin.contents).unwrap();
        assert!(script.contains("provision_file '1/sub/data.txt' '/sandbox/seed/sub/data.txt'"));
        assert!(
            ext4_image_has_directory(&overlay, "/upper/.openshell-provision/gen1/1/sub").unwrap()
        );

        // The guest has not copied gen1 yet, so a restart stages everything
        // again, beside it.
        let dropin = stage_files(&overlay, &files, &config, false, "gen2", 1000, 1000)
            .unwrap()
            .expect("a pending stage is redone");
        let script = String::from_utf8(dropin.contents).unwrap();
        assert!(script.contains("/sandbox/seed"), "{script}");
        assert!(ext4_image_has_directory(&overlay, "/upper/.openshell-provision/gen2").unwrap());

        // Nothing is staged for a sandbox without files.
        assert!(
            stage_files(&overlay, &[], &config, true, "gen3", 1000, 1000)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_dropin_refuses_to_replace_a_directory_with_a_file() {
        let guest = tempfile::tempdir().unwrap();
        let root = guest.path();
        fs::create_dir_all(root.join(".openshell-provision/g")).unwrap();
        fs::write(root.join(".openshell-provision/g/0"), "x").unwrap();
        fs::create_dir_all(root.join("etc/conf")).unwrap();
        let file = resolved("/etc/conf", FileKind::File, false);
        let script = render_dropin("g", &[&file], 0, 0);
        let script_path = guest.path().join("dropin.sh");
        fs::write(&script_path, script).unwrap();
        let output = std::process::Command::new("sh")
            .arg(&script_path)
            .env("ROOT_PREFIX", root)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("is a directory"));
        assert!(root.join("etc/conf").is_dir());
    }
}
