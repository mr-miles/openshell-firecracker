// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Where sandbox images come from (not in upstream).
//!
//! An image name with a registry host (`192.168.1.239:5000/app`, `ghcr.io/org/app`) always
//! comes from that registry, so a stale local copy can never stand in for it. A name without
//! one (`app:latest`, `localhost/app`) only comes from the local container engine chosen by
//! `OPENSHELL_VM_LOCAL_IMAGE_SOURCE` and `OPENSHELL_VM_LOCAL_IMAGE_SOURCE_SOCKET`; there is no
//! implicit Docker Hub.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Selects the local container engine: `auto`, `docker`, `podman` or `off`.
pub const LOCAL_IMAGE_SOURCE_ENV: &str = "OPENSHELL_VM_LOCAL_IMAGE_SOURCE";
/// Overrides the mode's default socket.
pub const LOCAL_IMAGE_SOURCE_SOCKET_ENV: &str = "OPENSHELL_VM_LOCAL_IMAGE_SOURCE_SOCKET";
/// Docker's default socket.
pub const DOCKER_DEFAULT_SOCKET: &str = "/var/run/docker.sock";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalImageSourceMode {
    /// Docker's default socket, then Podman's.
    #[default]
    Auto,
    Docker,
    Podman,
    /// No local images: every image name needs a registry host.
    Off,
}

impl LocalImageSourceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Off => "off",
        }
    }
}

impl FromStr for LocalImageSourceMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "docker" => Ok(Self::Docker),
            "podman" => Ok(Self::Podman),
            "off" => Ok(Self::Off),
            _ => Err(format!(
                "{LOCAL_IMAGE_SOURCE_ENV} must be auto, docker, podman or off (got '{value}')"
            )),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalImageSourceConfig {
    #[serde(default)]
    pub mode: LocalImageSourceMode,
    /// Replaces the mode's default sockets. Not allowed with `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
}

impl LocalImageSourceConfig {
    /// Parse the two settings. An empty socket counts as unset.
    pub fn parse(mode: &str, socket: Option<&str>) -> Result<Self, String> {
        let config = Self {
            mode: mode.parse()?,
            socket: socket
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(parse_socket)
                .transpose()?,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(socket) = &self.socket {
            if self.mode == LocalImageSourceMode::Off {
                return Err(format!(
                    "{LOCAL_IMAGE_SOURCE_SOCKET_ENV} is set but {LOCAL_IMAGE_SOURCE_ENV} is off; unset one of them"
                ));
            }
            if !socket.is_absolute() {
                return Err(format!(
                    "{LOCAL_IMAGE_SOURCE_SOCKET_ENV} must be an absolute path (got '{}')",
                    socket.display()
                ));
            }
        }
        Ok(())
    }

    /// The sockets to try, in order. Empty when local images are off.
    pub fn socket_candidates(&self) -> Vec<PathBuf> {
        self.socket_candidates_for(
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            rustix::process::geteuid().as_raw(),
        )
    }

    fn socket_candidates_for(&self, xdg_runtime_dir: Option<&OsStr>, uid: u32) -> Vec<PathBuf> {
        if self.mode == LocalImageSourceMode::Off {
            return Vec::new();
        }
        if let Some(socket) = &self.socket {
            return vec![socket.clone()];
        }
        let mut candidates = Vec::new();
        if self.mode != LocalImageSourceMode::Podman {
            candidates.push(PathBuf::from(DOCKER_DEFAULT_SOCKET));
        }
        if self.mode != LocalImageSourceMode::Docker {
            if let Some(dir) = xdg_runtime_dir.filter(|dir| !dir.is_empty()) {
                candidates.push(Path::new(dir).join("podman/podman.sock"));
            }
            let fallback = PathBuf::from(format!("/run/user/{uid}/podman/podman.sock"));
            if !candidates.contains(&fallback) {
                candidates.push(fallback);
            }
        }
        candidates
    }
}

/// Accepts a path or a `unix://` URL. Other schemes (`tcp://`, `ssh://`) are rejected, since
/// only a local socket is supported.
fn parse_socket(value: &str) -> Result<PathBuf, String> {
    let path = match value.strip_prefix("unix://") {
        Some(path) => path,
        None if value.contains("://") => {
            return Err(format!(
                "{LOCAL_IMAGE_SOURCE_SOCKET_ENV} must be a socket path or a unix:// URL (got '{value}')"
            ));
        }
        None => value,
    };
    Ok(PathBuf::from(path))
}

/// Whether an image name starts with a registry host.
///
/// That is a first path component that contains a `.` or a `:` (a port). Unlike Docker's rule, a bare `localhost/` is not a host, since
/// that is where Podman keeps images built locally.
pub fn image_ref_has_registry_host(image_ref: &str) -> bool {
    image_ref
        .split_once('/')
        .is_some_and(|(first, _)| first.contains('.') || first.contains(':'))
}

/// The fully qualified Docker Hub name for a name without a host, for error messages.
pub fn docker_hub_name(image_ref: &str) -> String {
    if image_ref.contains('/') {
        format!("docker.io/{image_ref}")
    } else {
        format!("docker.io/library/{image_ref}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_host_needs_a_dot_or_port_before_the_first_slash() {
        for name in [
            "192.168.1.239:5000/app:latest",
            "ghcr.io/org/app:1.0",
            "localhost:5000/app",
            "docker.io/library/ubuntu:24.04",
            "registry.example.com/app@sha256:abc",
        ] {
            assert!(image_ref_has_registry_host(name), "{name}");
        }
        for name in [
            "app",
            "app:latest",
            "app@sha256:abc",
            "myuser/app:latest",
            "localhost/my-image:latest",
            "openshell/sandbox-from:123",
        ] {
            assert!(!image_ref_has_registry_host(name), "{name}");
        }
    }

    #[test]
    fn docker_hub_name_adds_library_for_official_images() {
        assert_eq!(
            docker_hub_name("ubuntu:24.04"),
            "docker.io/library/ubuntu:24.04"
        );
        assert_eq!(docker_hub_name("myuser/app"), "docker.io/myuser/app");
    }

    #[test]
    fn parse_accepts_modes_and_socket_forms() {
        let config = LocalImageSourceConfig::parse(" Podman ", Some("unix:///tmp/p.sock")).unwrap();
        assert_eq!(config.mode, LocalImageSourceMode::Podman);
        assert_eq!(config.socket, Some(PathBuf::from("/tmp/p.sock")));

        let config = LocalImageSourceConfig::parse("docker", Some("/tmp/d.sock")).unwrap();
        assert_eq!(config.socket, Some(PathBuf::from("/tmp/d.sock")));

        let config = LocalImageSourceConfig::parse("off", Some("  ")).unwrap();
        assert_eq!(
            config,
            LocalImageSourceConfig {
                mode: LocalImageSourceMode::Off,
                socket: None,
            }
        );
    }

    #[test]
    fn parse_rejects_bad_values() {
        for (mode, socket, expected) in [
            ("dockr", None, "must be auto, docker, podman or off"),
            ("off", Some("/tmp/d.sock"), "unset one of them"),
            ("docker", Some("tcp://127.0.0.1:2375"), "unix:// URL"),
            ("docker", Some("ssh://host"), "unix:// URL"),
            ("podman", Some("podman.sock"), "absolute path"),
            ("podman", Some("unix://podman.sock"), "absolute path"),
        ] {
            let err = LocalImageSourceConfig::parse(mode, socket).unwrap_err();
            assert!(err.contains(expected), "{mode} {socket:?}: {err}");
        }
    }

    #[test]
    fn socket_candidates_follow_the_mode() {
        let xdg = Some(OsStr::new("/run/user/1000"));
        let candidates = |mode: &str, socket: Option<&str>, xdg: Option<&OsStr>| {
            LocalImageSourceConfig::parse(mode, socket)
                .unwrap()
                .socket_candidates_for(xdg, 1000)
        };
        let paths = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();

        assert_eq!(
            candidates("auto", None, xdg),
            paths(&["/var/run/docker.sock", "/run/user/1000/podman/podman.sock"])
        );
        assert_eq!(
            candidates("podman", None, Some(OsStr::new("/xdg"))),
            paths(&[
                "/xdg/podman/podman.sock",
                "/run/user/1000/podman/podman.sock"
            ])
        );
        assert_eq!(
            candidates("podman", None, None),
            paths(&["/run/user/1000/podman/podman.sock"])
        );
        assert_eq!(
            candidates("docker", None, xdg),
            paths(&["/var/run/docker.sock"])
        );
        assert_eq!(
            candidates("auto", Some("/tmp/x.sock"), xdg),
            paths(&["/tmp/x.sock"])
        );
        assert!(candidates("off", None, xdg).is_empty());
    }

    #[test]
    fn config_defaults_to_auto_when_absent_from_worker_json() {
        let config: LocalImageSourceConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, LocalImageSourceConfig::default());
        assert_eq!(config.mode, LocalImageSourceMode::Auto);
    }
}
