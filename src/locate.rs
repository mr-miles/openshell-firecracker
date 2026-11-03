// SPDX-License-Identifier: Apache-2.0

//! Find the files the plugin needs: Firecracker, the guest kernel, and tools such
//! as the OpenShell gateway.
//!
//! An explicit environment variable always wins, and if it is wrong that is an
//! error: it never falls back silently to something else. Without one, look
//! beside the launcher (where `install.sh` puts the kernel), then on `PATH`, then
//! in the usual install directories.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::launcher::{ENV_BIN, ENV_KERNEL};

/// Where a located file came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The named environment variable.
    Env(&'static str),
    /// A default location.
    Default,
}

impl Source {
    /// For reports.
    pub fn describe(&self) -> String {
        match self {
            Self::Env(name) => format!("from {name}"),
            Self::Default => "found in a default location".to_string(),
        }
    }
}

/// A located file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub source: Source,
}

/// Directories searched for Firecracker after the launcher's own and `PATH`.
pub const FIRECRACKER_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin", "/opt/firecracker"];

/// Directories searched for the guest kernel after the launcher's own.
pub const KERNEL_DIRS: &[&str] = &[
    "/usr/local/libexec/openshell-firecracker",
    "/var/lib/openshell-firecracker",
];

/// What to look for and where.
#[derive(Debug, Clone)]
pub struct Search<'a> {
    /// Variable that overrides the search, if there is one.
    pub env_name: Option<&'static str>,
    /// Its value. An empty value counts as unset.
    pub env_value: Option<&'a OsStr>,
    pub file_name: &'a str,
    /// The launcher's own directory.
    pub beside: Option<&'a Path>,
    /// `PATH`, when the file may be found there.
    pub path_var: Option<&'a OsStr>,
    pub dirs: &'a [&'a str],
}

/// Run a search.
pub fn locate(search: &Search<'_>) -> Result<Found, String> {
    if let (Some(name), Some(value)) = (search.env_name, search.env_value)
        && !value.is_empty()
    {
        let path = PathBuf::from(value);
        if !path.is_absolute() || !path.is_file() {
            return Err(format!(
                "{name} must be an absolute path to an existing file: {}",
                path.display()
            ));
        }
        return Ok(Found {
            path,
            source: Source::Env(name),
        });
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = search.beside {
        candidates.push(dir.join(search.file_name));
    }
    if let Some(path_var) = search.path_var {
        candidates.extend(std::env::split_paths(path_var).map(|dir| dir.join(search.file_name)));
    }
    candidates.extend(
        search
            .dirs
            .iter()
            .map(|dir| Path::new(dir).join(search.file_name)),
    );

    let mut tried: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        // A relative PATH entry depends on the working directory: skip it.
        if !candidate.is_absolute() || tried.contains(&candidate) {
            continue;
        }
        if candidate.is_file() {
            return Ok(Found {
                path: candidate,
                source: Source::Default,
            });
        }
        tried.push(candidate);
    }

    let looked = tried
        .iter()
        .filter_map(|path| path.parent())
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(match search.env_name {
        Some(name) => format!(
            "{name} is not set and {} was not found (looked in: {looked})",
            search.file_name
        ),
        None => format!("{} was not found (looked in: {looked})", search.file_name),
    })
}

fn env_value(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// The directory holding the running launcher.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

/// Firecracker: `OPENSHELL_VM_FIRECRACKER_BIN`, else beside the launcher, on
/// `PATH`, or in a standard directory.
pub fn firecracker(beside: Option<&Path>) -> Result<Found, String> {
    let value = env_value(ENV_BIN);
    let path_var = std::env::var_os("PATH");
    locate(&Search {
        env_name: Some(ENV_BIN),
        env_value: value.as_deref(),
        file_name: "firecracker",
        beside,
        path_var: path_var.as_deref(),
        dirs: FIRECRACKER_DIRS,
    })
}

/// The guest kernel: `OPENSHELL_VM_FIRECRACKER_KERNEL`, else `vmlinux` beside the
/// launcher or in a standard directory.
pub fn kernel(beside: Option<&Path>) -> Result<Found, String> {
    let value = env_value(ENV_KERNEL);
    locate(&Search {
        env_name: Some(ENV_KERNEL),
        env_value: value.as_deref(),
        file_name: "vmlinux",
        beside,
        path_var: None,
        dirs: KERNEL_DIRS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"").unwrap();
        path
    }

    fn search<'a>(
        env_value: Option<&'a OsStr>,
        beside: Option<&'a Path>,
        path_var: Option<&'a OsStr>,
        dirs: &'a [&'a str],
    ) -> Search<'a> {
        Search {
            env_name: Some("FC_TEST"),
            env_value,
            file_name: "firecracker",
            beside,
            path_var,
            dirs,
        }
    }

    #[test]
    fn the_environment_variable_wins() {
        let dir = tempfile::tempdir().unwrap();
        let chosen = touch(dir.path(), "chosen");
        let beside = tempfile::tempdir().unwrap();
        touch(beside.path(), "firecracker");
        let found = locate(&search(Some(chosen.as_os_str()), Some(beside.path()), None, &[]))
            .unwrap();
        assert_eq!(found.path, chosen);
        assert_eq!(found.source, Source::Env("FC_TEST"));
    }

    #[test]
    fn a_wrong_environment_variable_is_an_error_not_a_fallback() {
        let beside = tempfile::tempdir().unwrap();
        touch(beside.path(), "firecracker");
        let error = locate(&search(
            Some(OsStr::new("/nonexistent/firecracker")),
            Some(beside.path()),
            None,
            &[],
        ))
        .unwrap_err();
        assert!(error.contains("FC_TEST"), "{error}");
        let relative = locate(&search(Some(OsStr::new("firecracker")), None, None, &[]));
        assert!(relative.is_err(), "relative paths are rejected");
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        let beside = tempfile::tempdir().unwrap();
        let expected = touch(beside.path(), "firecracker");
        let found = locate(&search(Some(OsStr::new("")), Some(beside.path()), None, &[])).unwrap();
        assert_eq!(found.path, expected);
        assert_eq!(found.source, Source::Default);
    }

    #[test]
    fn beside_the_launcher_comes_before_path() {
        let beside = tempfile::tempdir().unwrap();
        let on_path = tempfile::tempdir().unwrap();
        let expected = touch(beside.path(), "firecracker");
        touch(on_path.path(), "firecracker");
        let path_var = std::env::join_paths([on_path.path()]).unwrap();
        let found = locate(&search(None, Some(beside.path()), Some(&path_var), &[])).unwrap();
        assert_eq!(found.path, expected);
    }

    #[test]
    fn path_and_standard_directories_are_searched() {
        let on_path = tempfile::tempdir().unwrap();
        let expected = touch(on_path.path(), "firecracker");
        let path_var = std::env::join_paths([on_path.path()]).unwrap();
        assert_eq!(
            locate(&search(None, None, Some(&path_var), &[])).unwrap().path,
            expected
        );

        let standard = tempfile::tempdir().unwrap();
        let expected = touch(standard.path(), "firecracker");
        let dir = standard.path().to_str().unwrap();
        assert_eq!(
            locate(&search(None, None, None, &[dir])).unwrap().path,
            expected
        );
    }

    #[test]
    fn relative_path_entries_are_skipped() {
        let path_var = OsString::from("relative/bin:");
        assert!(locate(&search(None, None, Some(&path_var), &[])).is_err());
    }

    #[test]
    fn a_miss_names_the_variable_and_where_it_looked() {
        let empty = tempfile::tempdir().unwrap();
        let error = locate(&search(None, Some(empty.path()), None, &[])).unwrap_err();
        assert!(error.contains("FC_TEST is not set"), "{error}");
        assert!(error.contains(&empty.path().display().to_string()), "{error}");
    }
}
