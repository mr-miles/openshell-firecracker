// SPDX-License-Identifier: Apache-2.0

//! Operator pre-flight: `openshell-firecracker-launcher check`.
//!
//! The stock VM driver knows nothing about Firecracker, so it cannot fail at
//! startup when the host is not ready. Run this before enabling the driver,
//! with the same environment as the driver's service.

use std::path::Path;
use std::process::Command;

use crate::config::{MIN_FIRECRACKER_VERSION, parse_firecracker_version};
use crate::confine;
use crate::launcher::HostConfig;
use crate::subids::{self, SubuidMode};

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    Ok(String),
    Warn(String),
    Fail(String),
}

/// Check that `firecracker` runs and is new enough for the generated config.
pub fn check_firecracker_version(binary: &Path) -> Result<String, String> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|error| format!("run {} --version: {error}", binary.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = parse_firecracker_version(&text).ok_or_else(|| {
        format!(
            "cannot read a Firecracker version from `{} --version`",
            binary.display()
        )
    })?;
    let (major, minor, patch) = version;
    if version < MIN_FIRECRACKER_VERSION {
        let (min_major, min_minor, min_patch) = MIN_FIRECRACKER_VERSION;
        return Err(format!(
            "Firecracker {major}.{minor}.{patch} is too old; \
             {min_major}.{min_minor}.{min_patch} or newer is required"
        ));
    }
    Ok(format!("Firecracker {major}.{minor}.{patch}"))
}

fn tool_present(name: &str) -> bool {
    // Both tools exit nonzero without a device argument; only a failure to
    // start the program means it is missing.
    Command::new(name).arg("-V").output().is_ok()
}

/// Run every check and collect the findings.
pub fn run_checks(host: &Result<HostConfig, String>, launcher: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    let host = match host {
        Ok(host) => {
            findings.push(Finding::Ok(format!(
                "firecracker: {}; kernel: {}",
                host.firecracker.display(),
                host.kernel.display()
            )));
            Some(host)
        }
        Err(error) => {
            findings.push(Finding::Fail(error.clone()));
            None
        }
    };

    if let Some(host) = host {
        match check_firecracker_version(&host.firecracker) {
            Ok(version) => findings.push(Finding::Ok(version)),
            Err(error) => findings.push(Finding::Fail(error)),
        }
    }

    match confine::probe(launcher) {
        Ok(()) => findings.push(Finding::Ok(
            "KVM is accessible and the VMM jail can be built".to_string(),
        )),
        Err(error) => findings.push(Finding::Fail(format!(
            "host cannot run a jailed VMM: {error}"
        ))),
    }

    for tool in ["debugfs", "mke2fs"] {
        if tool_present(tool) {
            findings.push(Finding::Ok(format!("{tool} is installed")));
        } else {
            findings.push(Finding::Fail(format!(
                "{tool} is missing (package e2fsprogs)"
            )));
        }
    }

    if let Some(host) = host {
        findings.push(match host.subuids {
            SubuidMode::Off => Finding::Ok("sub-uid isolation is off".to_string()),
            mode => match subids::preflight(&host.firecracker, &host.kernel) {
                Ok(()) => Finding::Ok("sub-uid isolation is available".to_string()),
                Err(error) if mode == SubuidMode::Required => Finding::Fail(format!(
                    "sub-uid isolation is required but unavailable: {error}"
                )),
                Err(error) => Finding::Warn(format!(
                    "sub-uid isolation unavailable; VMMs will run as the driver user: {error}"
                )),
            },
        });
    }
    findings
}

/// Print the report; the process exit code (0 unless something failed).
pub fn run(launcher: &Path) -> i32 {
    let host = HostConfig::from_env();
    let findings = run_checks(&host, launcher);
    let mut failed = false;
    for finding in &findings {
        match finding {
            Finding::Ok(text) => println!("ok    {text}"),
            Finding::Warn(text) => println!("warn  {text}"),
            Finding::Fail(text) => {
                failed = true;
                println!("FAIL  {text}");
            }
        }
    }
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_firecracker_is_a_failure() {
        assert!(check_firecracker_version(Path::new("/nonexistent/firecracker")).is_err());
    }

    #[test]
    fn an_unconfigured_host_reports_failures_instead_of_panicking() {
        let findings = run_checks(
            &Err("OPENSHELL_VM_FIRECRACKER_BIN is not set".to_string()),
            Path::new("/nonexistent/launcher"),
        );
        assert!(
            findings
                .iter()
                .any(|f| matches!(f, Finding::Fail(text) if text.contains("is not set")))
        );
        assert!(findings.iter().any(|f| matches!(f, Finding::Fail(_))));
    }
}
