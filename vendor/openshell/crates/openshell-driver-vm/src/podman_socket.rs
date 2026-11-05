// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Find a local Podman API socket, so a sandbox image can be taken from a local
//! Podman image store.
//!
//! This is the native-Linux part of `OpenShell`'s Podman socket discovery: the
//! well-known socket locations, then the connection the `podman` CLI reports.
//! Podman machines (the VM-backed installs on macOS and Windows) are not handled.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Return the first responsive well-known socket, then ask the Podman CLI for
/// the active connection.
pub fn detect_socket() -> Option<PathBuf> {
    openshell_core::local_api_socket::first_responsive_socket(&socket_candidates(), |response| {
        openshell_core::local_api_socket::http_response_is_success(response)
            && openshell_core::local_api_socket::contains_ascii(response, b"Libpod-Api-Version:")
    })
    .or_else(discover_socket)
}

fn socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env_var_nonempty("OPENSHELL_PODMAN_SOCKET") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime_dir).join("podman/podman.sock"));
    }
    candidates.push(PathBuf::from(format!(
        "/run/user/{}/podman/podman.sock",
        rustix::process::geteuid().as_raw()
    )));
    if let Some(home) = std::env::var_os("HOME") {
        candidates
            .push(PathBuf::from(home).join(".local/share/containers/podman/machine/podman.sock"));
    }
    candidates
}

/// Query the connection the Podman CLI selects. A remote service (a machine) is
/// not supported and yields `None`.
fn discover_socket() -> Option<PathBuf> {
    let stdout = run_bounded_command("podman", &["info", "--format", "json"], DISCOVERY_TIMEOUT)?;

    // A successful `podman info` proves an explicit socket is usable. A named
    // connection has higher precedence, so only honor CONTAINER_HOST directly
    // when no CONTAINER_CONNECTION is set.
    if env_var_nonempty("CONTAINER_CONNECTION").is_none()
        && let Some(path) = env_var_nonempty("CONTAINER_HOST")
            .as_deref()
            .and_then(unix_url_socket_path)
    {
        return Some(path);
    }

    let info: serde_json::Value = serde_json::from_slice(&stdout).ok()?;
    if info["host"]["serviceIsRemote"].as_bool().unwrap_or(false) {
        return None;
    }
    parse_info_socket(&info)
}

fn parse_info_socket(info: &serde_json::Value) -> Option<PathBuf> {
    let path = info["host"]["remoteSocket"]["path"].as_str()?;
    unix_url_socket_path(path).or_else(|| (!path.is_empty()).then(|| PathBuf::from(path)))
}

fn unix_url_socket_path(url: &str) -> Option<PathBuf> {
    let path = url.trim().strip_prefix("unix://")?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn env_var_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Capture stdout without allowing a stalled `podman`, or a descendant holding
/// the stdout pipe open, to block driver startup forever.
fn run_bounded_command(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;
    use std::sync::mpsc;

    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().ok()?;

    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output);
        let _ = sender.send(output);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(DISCOVERY_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => break None,
        }
    };

    let Some(status) = status else {
        terminate_process_group(&mut child);
        let _ = child.wait();
        return None;
    };

    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(output) if status.success() => Some(output),
        Ok(_) => None,
        Err(_) => {
            terminate_process_group(&mut child);
            None
        }
    }
}

fn terminate_process_group(child: &mut std::process::Child) {
    let pid = i32::try_from(child.id()).unwrap_or(i32::MAX);
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_info_socket_rejects_missing_or_empty_paths() {
        assert_eq!(
            parse_info_socket(&json!({"host": {"remoteSocket": {}}})),
            None
        );
        assert_eq!(
            parse_info_socket(&json!({"host": {"remoteSocket": {"path": ""}}})),
            None
        );
    }

    #[test]
    fn parses_native_socket_paths() {
        let info = json!({"host": {"remoteSocket": {"path": "unix:///run/user/1000/podman.sock"}}});
        assert_eq!(
            parse_info_socket(&info),
            Some(PathBuf::from("/run/user/1000/podman.sock"))
        );
        let bare = json!({"host": {"remoteSocket": {"path": "/run/podman/podman.sock"}}});
        assert_eq!(
            parse_info_socket(&bare),
            Some(PathBuf::from("/run/podman/podman.sock"))
        );
    }

    #[test]
    fn unix_url_socket_path_only_accepts_nonempty_unix_urls() {
        assert_eq!(
            unix_url_socket_path("unix:///run/user/1000/podman.sock"),
            Some(PathBuf::from("/run/user/1000/podman.sock"))
        );
        assert_eq!(unix_url_socket_path("ssh://core@127.0.0.1/x"), None);
        assert_eq!(unix_url_socket_path("tcp://127.0.0.1:2375"), None);
        assert_eq!(unix_url_socket_path("unix://"), None);
    }

    #[test]
    fn candidates_prefer_the_explicit_override() {
        temp_env::with_vars(
            [
                ("OPENSHELL_PODMAN_SOCKET", Some("/explicit.sock")),
                ("XDG_RUNTIME_DIR", Some("/run/user/1")),
            ],
            || {
                let candidates = socket_candidates();
                assert_eq!(candidates[0], PathBuf::from("/explicit.sock"));
                assert_eq!(
                    candidates[1],
                    PathBuf::from("/run/user/1/podman/podman.sock")
                );
            },
        );
    }

    #[test]
    fn bounded_command_captures_stdout_on_success() {
        assert_eq!(
            run_bounded_command("printf", &["hello"], Duration::from_secs(5)),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn bounded_command_returns_none_on_nonzero_exit_or_missing_program() {
        assert_eq!(
            run_bounded_command("false", &[], Duration::from_secs(5)),
            None
        );
        assert_eq!(
            run_bounded_command(
                "openshell-nonexistent-binary-xyz",
                &[],
                Duration::from_secs(5),
            ),
            None
        );
    }

    #[test]
    fn bounded_command_kills_child_that_exceeds_deadline() {
        let start = Instant::now();
        let result = run_bounded_command("sleep", &["30"], Duration::from_millis(200));
        assert_eq!(result, None);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "bounded command did not return promptly"
        );
    }

    #[test]
    fn bounded_command_bounds_drain_when_in_group_descendant_holds_stdout() {
        let start = Instant::now();
        let result = run_bounded_command(
            "sh",
            &["-c", "sleep 30 & echo done"],
            Duration::from_millis(300),
        );
        assert_eq!(result, None);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "drain blocked on a descendant holding stdout"
        );
    }

    #[test]
    fn bounded_command_bounds_drain_when_descendant_escapes_process_group() {
        let start = Instant::now();
        let result = run_bounded_command(
            "bash",
            &["-c", "set -m; sleep 5 & echo done"],
            Duration::from_millis(300),
        );
        assert_eq!(result, None);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "drain blocked on a descendant that escaped the process group"
        );
    }
}
