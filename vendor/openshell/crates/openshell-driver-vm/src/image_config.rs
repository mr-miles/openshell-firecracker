// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runtime settings a container image declares: `ENV` and `WORKDIR`.
//!
//! A container runtime starts the workload with the image's `Config.Env` and in
//! its `Config.WorkingDir`. The VM driver unpacks the image into a disk and
//! used to ignore both, so every workload started in a fixed `/sandbox` with
//! only the sandbox spec's environment. This module reads the two settings from
//! the OCI image configuration (or a local Docker image's inspect data), keeps
//! them beside the prepared disk, and merges them into what the workload gets.
//!
//! The image is untrusted input. Entries that could not be set safely are
//! dropped, and the working directory must be a plain absolute path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File beside a prepared image disk that holds its [`ImageRuntimeConfig`].
pub const IMAGE_RUNTIME_CONFIG_FILE: &str = "image-config.json";

/// Prefix the supervisor reserves for its own variables. An image cannot set
/// these; the supervisor strips them from workloads as well.
const RESERVED_ENV_PREFIX: &str = "OPENSHELL_";

/// Directories the guest owns or mounts over. An image cannot make these the
/// workload's working directory.
const RESERVED_WORKDIRS: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/opt/openshell",
    "/.openshell",
    "/.openshell-bootstrap",
];

/// What the image declares for its workload. Both parts are already validated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRuntimeConfig {
    /// `KEY=value` entries in image order.
    #[serde(default)]
    pub env: Vec<String>,
    /// Normalized absolute working directory. `None` when the image sets none
    /// (a container then starts in `/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

#[derive(Deserialize)]
struct OciImageConfigDocument {
    #[serde(default)]
    config: Option<OciContainerConfig>,
}

#[derive(Deserialize)]
struct OciContainerConfig {
    #[serde(default, rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(default, rename = "WorkingDir")]
    working_dir: Option<String>,
}

impl ImageRuntimeConfig {
    /// Read the settings from an OCI image configuration blob.
    pub fn from_oci_config(blob: &[u8]) -> Result<Self, String> {
        let document: OciImageConfigDocument = serde_json::from_slice(blob)
            .map_err(|error| format!("parse OCI image configuration: {error}"))?;
        let Some(config) = document.config else {
            return Ok(Self::default());
        };
        Ok(Self::from_declared(
            config.env.unwrap_or_default(),
            config.working_dir.as_deref(),
        ))
    }

    /// Build from the raw `Env` and `WorkingDir` an image declares, dropping
    /// whatever cannot be applied safely.
    pub fn from_declared(env: Vec<String>, working_dir: Option<&str>) -> Self {
        let mut accepted: Vec<String> = Vec::with_capacity(env.len());
        for entry in env {
            if !env_entry_is_acceptable(&entry) {
                tracing::warn!(
                    name = entry.split('=').next().unwrap_or_default(),
                    "vm driver: ignoring image ENV entry that cannot be applied"
                );
                continue;
            }
            accepted.push(entry);
        }
        let working_dir = working_dir.and_then(|raw| match normalize_working_dir(raw) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(error, "vm driver: ignoring image WORKDIR");
                None
            }
        });
        Self {
            env: accepted,
            working_dir,
        }
    }

    /// The working directory the workload gets: the image's, else `default`
    /// (an operator-set directory that is already validated).
    pub fn effective_working_dir(&self, default: Option<&str>) -> Option<String> {
        self.working_dir
            .clone()
            .or_else(|| default.map(ToString::to_string))
    }

    /// The image environment as a map. A repeated name keeps its last value,
    /// as a container runtime does.
    pub fn env_map(&self) -> HashMap<String, String> {
        self.env
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// Read the sidecar in `cache_dir`. A prepared image from before the
    /// sidecar existed, or one built from a bare rootfs archive, has none and
    /// declares nothing.
    pub fn load(cache_dir: &Path) -> Result<Self, String> {
        let path = cache_dir.join(IMAGE_RUNTIME_CONFIG_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("parse {}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        }
    }

    /// Write the sidecar into `cache_dir` through a temporary file, so a reader
    /// never sees a partial document.
    pub fn store(&self, cache_dir: &Path) -> Result<PathBuf, String> {
        let destination = cache_dir.join(IMAGE_RUNTIME_CONFIG_FILE);
        let temporary = cache_dir.join(format!(
            ".{IMAGE_RUNTIME_CONFIG_FILE}.{}.tmp",
            std::process::id()
        ));
        let bytes = serde_json::to_vec(self)
            .map_err(|error| format!("encode image runtime configuration: {error}"))?;
        std::fs::write(&temporary, bytes)
            .and_then(|()| std::fs::rename(&temporary, &destination))
            .map_err(|error| format!("write {}: {error}", destination.display()))?;
        Ok(destination)
    }
}

/// The environment a workload starts with: the image's, overridden by the
/// sandbox's own (its template, then its spec).
pub fn layered_environment(
    image: &ImageRuntimeConfig,
    sandbox: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut environment = image.env_map();
    environment.extend(sandbox);
    environment
}

/// Validate the operator's default working directory and return it normalized.
/// Unlike an image's, a bad one is an error, and `/` is not a usable default.
pub fn validate_default_working_dir(raw: &str) -> Result<String, String> {
    normalize_working_dir(raw)?
        .ok_or_else(|| format!("default workdir must be a directory below /, got {raw:?}"))
}

fn env_entry_is_acceptable(entry: &str) -> bool {
    let Some((name, value)) = entry.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(RESERVED_ENV_PREFIX)
        && !name.contains('\0')
        && !value.contains('\0')
}

/// Validate and normalize an image `WorkingDir`.
///
/// Returns `Ok(None)` for an empty value or `/`: the workload then starts in
/// `/` without that directory being made writable.
fn normalize_working_dir(raw: &str) -> Result<Option<String>, String> {
    let raw = raw.trim_end_matches('/');
    if raw.is_empty() {
        return Ok(None);
    }
    if !raw.starts_with('/') {
        return Err(format!("WORKDIR is not absolute: {raw:?}"));
    }
    if raw.contains(['\0', '\n', '\r']) {
        return Err("WORKDIR contains a control character".to_string());
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("WORKDIR contains '..': {raw:?}")),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let normalized = format!("/{}", parts.join("/"));
    for reserved in RESERVED_WORKDIRS {
        if normalized == *reserved || normalized.starts_with(&format!("{reserved}/")) {
            return Err(format!("WORKDIR {normalized} is reserved by the guest"));
        }
    }
    Ok(Some(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(env: &[&str], working_dir: Option<&str>) -> ImageRuntimeConfig {
        ImageRuntimeConfig::from_declared(
            env.iter().map(ToString::to_string).collect(),
            working_dir,
        )
    }

    #[test]
    fn reads_env_and_workdir_from_an_oci_config() {
        let blob = br#"{"architecture":"amd64","config":{"Env":["PATH=/usr/local/go/bin:/usr/bin","GOPATH=/go"],"WorkingDir":"/go/src/app","User":"app"}}"#;
        let config = ImageRuntimeConfig::from_oci_config(blob).unwrap();
        assert_eq!(
            config.env,
            ["PATH=/usr/local/go/bin:/usr/bin", "GOPATH=/go"]
        );
        assert_eq!(config.working_dir.as_deref(), Some("/go/src/app"));
    }

    #[test]
    fn an_image_that_declares_nothing_yields_nothing() {
        for blob in [
            &b"{}"[..],
            br#"{"config":{}}"#,
            br#"{"config":{"Env":null}}"#,
        ] {
            assert_eq!(
                ImageRuntimeConfig::from_oci_config(blob).unwrap(),
                ImageRuntimeConfig::default()
            );
        }
        assert!(ImageRuntimeConfig::from_oci_config(b"not json").is_err());
    }

    #[test]
    fn workdir_is_normalized() {
        assert_eq!(
            declared(&[], Some("/app/")).working_dir.as_deref(),
            Some("/app")
        );
        assert_eq!(
            declared(&[], Some("//a/./b//")).working_dir.as_deref(),
            Some("/a/b")
        );
    }

    #[test]
    fn root_and_empty_workdir_mean_no_workdir() {
        for raw in [None, Some(""), Some("/"), Some("///")] {
            assert_eq!(declared(&[], raw).working_dir, None, "{raw:?}");
        }
    }

    #[test]
    fn unsafe_workdirs_are_dropped() {
        for raw in [
            "relative/path",
            "/a/../etc",
            "/a\nb",
            "/proc",
            "/sys/kernel",
            "/dev/shm",
            "/opt/openshell/bin",
            "/.openshell-bootstrap",
        ] {
            assert_eq!(declared(&[], Some(raw)).working_dir, None, "{raw:?}");
        }
        // A look-alike prefix is a different directory.
        assert_eq!(
            declared(&[], Some("/devices")).working_dir.as_deref(),
            Some("/devices")
        );
    }

    #[test]
    fn unusable_env_entries_are_dropped_and_the_rest_kept() {
        let config = declared(
            &[
                "KEEP=1",
                "NOEQUALS",
                "=novalue-name",
                "OPENSHELL_SANDBOX_TOKEN=stolen",
                "EMPTY=",
                "WITH_EQ=a=b",
            ],
            None,
        );
        assert_eq!(config.env, ["KEEP=1", "EMPTY=", "WITH_EQ=a=b"]);
    }

    #[test]
    fn a_repeated_name_keeps_the_last_value() {
        let map = declared(&["A=1", "A=2"], None).env_map();
        assert_eq!(map.get("A").map(String::as_str), Some("2"));
    }

    #[test]
    fn the_default_workdir_applies_only_when_the_image_has_none() {
        let with = declared(&[], Some("/app"));
        let without = declared(&[], None);
        assert_eq!(
            with.effective_working_dir(Some("/sandbox")).as_deref(),
            Some("/app")
        );
        assert_eq!(
            without.effective_working_dir(Some("/sandbox")).as_deref(),
            Some("/sandbox")
        );
        assert_eq!(without.effective_working_dir(None), None);
    }

    #[test]
    fn the_default_workdir_is_validated_and_normalized() {
        assert_eq!(
            validate_default_working_dir("/sandbox/").unwrap(),
            "/sandbox"
        );
        for bad in ["", "/", "relative", "/a/../b", "/proc", "/dev/shm"] {
            assert!(validate_default_working_dir(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_sandbox_environment_overrides_the_image() {
        let image = declared(&["A=image", "B=image"], None);
        let sandbox = HashMap::from([
            ("B".to_string(), "sandbox".to_string()),
            ("C".to_string(), "sandbox".to_string()),
        ]);
        let merged = layered_environment(&image, sandbox);
        assert_eq!(merged["A"], "image");
        assert_eq!(merged["B"], "sandbox");
        assert_eq!(merged["C"], "sandbox");
    }

    #[test]
    fn sidecar_round_trips_and_absence_means_nothing_declared() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ImageRuntimeConfig::load(dir.path()).unwrap(),
            ImageRuntimeConfig::default()
        );
        let config = declared(&["A=1"], Some("/work"));
        config.store(dir.path()).unwrap();
        assert_eq!(ImageRuntimeConfig::load(dir.path()).unwrap(), config);

        std::fs::write(dir.path().join(IMAGE_RUNTIME_CONFIG_FILE), b"{").unwrap();
        assert!(ImageRuntimeConfig::load(dir.path()).is_err());
    }
}
