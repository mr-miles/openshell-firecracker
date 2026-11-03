// SPDX-License-Identifier: Apache-2.0

//! Operator pre-flight: `openshell-firecracker-launcher check`.
//!
//! The stock VM driver knows nothing about Firecracker, so it cannot fail at
//! startup when the host is not ready. This checks every step the operator was
//! asked to do by hand: OpenShell installed, the driver able to use this
//! launcher, Firecracker and the guest kernel present, the host able to run a
//! jailed VMM, the driver's environment, and the gateway's registration of the
//! driver. Run it with the same environment as the driver's service.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{MIN_FIRECRACKER_VERSION, parse_firecracker_version};
use crate::confine;
use crate::delegate::STOCK_DRIVER_ENV;
use crate::launcher::ENV_SUBUIDS;
use crate::locate::{self, Search};
use crate::subids::{self, SubuidMode};

/// The driver's variable naming the launcher it runs for every VM.
const LAUNCHER_ENV: &str = "OPENSHELL_VM_LAUNCHER_BIN";
const OPENSHELL_INSTALL: &str =
    "curl -LsSf https://raw.githubusercontent.com/NVIDIA/OpenShell/main/install.sh | sh";

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// Starts a group of checks.
    Heading(&'static str),
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

/// The first `major.minor.patch` in `text` (`openshell-gateway 0.1.6`, `v0.1.6-pre.1`).
pub fn first_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|token| {
            let mut parts = token.split('.');
            let major: u32 = parts.next()?.parse().ok()?;
            let minor: u32 = parts.next()?.parse().ok()?;
            let patch: u32 = parts.next()?.parse().ok()?;
            Some((major, minor, patch))
        })
}

/// What a kernel file turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelKind {
    /// Uncompressed `vmlinux`: what Firecracker boots on x86_64.
    Elf,
    /// Raw arm64 `Image`: what Firecracker boots on aarch64.
    ArmImage,
    /// x86 `bzImage`, which is compressed.
    BzImage,
    Gzip,
    Unknown,
}

/// Classify a kernel by its first bytes (at least 0x210 of them).
pub fn kernel_kind(head: &[u8]) -> KernelKind {
    if head.starts_with(b"\x7fELF") {
        KernelKind::Elf
    } else if head.starts_with(&[0x1f, 0x8b]) {
        KernelKind::Gzip
    } else if head.get(0x202..0x206) == Some(&b"HdrS"[..]) {
        KernelKind::BzImage
    } else if head.get(0x38..0x3c) == Some(&b"ARMd"[..]) {
        KernelKind::ArmImage
    } else {
        KernelKind::Unknown
    }
}

fn check_kernel_file(path: &Path) -> Finding {
    let mut head = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| file.take(0x400).read_to_end(&mut head));
    if let Err(error) = read {
        return Finding::Fail(format!(
            "cannot read the guest kernel {}: {error}",
            path.display()
        ));
    }
    let wants_elf = cfg!(target_arch = "x86_64");
    match (kernel_kind(&head), wants_elf) {
        (KernelKind::Elf, true) | (KernelKind::ArmImage, false) => {
            Finding::Ok("the guest kernel is the uncompressed format Firecracker boots".to_string())
        }
        (KernelKind::BzImage | KernelKind::Gzip, _) => Finding::Fail(format!(
            "{} is a compressed kernel; Firecracker needs the uncompressed vmlinux",
            path.display()
        )),
        (KernelKind::Elf, false) => Finding::Fail(format!(
            "{} is an ELF vmlinux; Firecracker on aarch64 needs the raw Image",
            path.display()
        )),
        (KernelKind::ArmImage, true) => Finding::Fail(format!(
            "{} is an arm64 Image but this host is x86_64",
            path.display()
        )),
        (KernelKind::Unknown, _) => Finding::Warn(format!(
            "{} is not a recognised kernel format; if the guest hangs at boot, check this first",
            path.display()
        )),
    }
}

/// Whether AppArmor confines unprivileged user namespaces on this host (Ubuntu
/// 23.10 and later), which breaks the jail unless a profile allows the launcher.
fn userns_restricted_by_apparmor() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|value| value.trim() == "1")
}

fn tool_present(name: &str) -> bool {
    // Both tools exit nonzero without a device argument; only a failure to
    // start the program means it is missing.
    Command::new(name).arg("-V").output().is_ok()
}

/// Stdout and stderr of `program arg`, or `None` if it will not run.
fn run_output(program: &Path, arg: &str) -> Option<String> {
    let output = Command::new(program).arg(arg).output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

fn env_text(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// What the gateway's configuration file says about this driver.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GatewayToml {
    pub compute_driver: Option<String>,
    pub socket_path: Option<String>,
    /// Any mention of `gateway_jwt`, the launch-authentication signing bundle.
    pub mentions_jwt: bool,
}

/// Read the few keys that matter, line by line. This is not a TOML parser: it
/// understands `[table]` headers and `key = "value"` lines, which is how the
/// gateway's configuration is written.
pub fn scan_gateway_toml(text: &str) -> GatewayToml {
    let mut found = GatewayToml::default();
    let mut table = String::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("gateway_jwt") {
            found.mentions_jwt = true;
        }
        if let Some(header) = line.strip_prefix('[') {
            table = header
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').to_string();
            match (table.as_str(), key.trim()) {
                ("openshell.gateway", "compute_driver") => found.compute_driver = Some(value),
                ("openshell.drivers.firecracker", "socket_path") => found.socket_path = Some(value),
                _ => {}
            }
        }
    }
    found
}

fn check_openshell(launcher: &Path, out: &mut Vec<Finding>) {
    out.push(Finding::Heading("OpenShell"));

    let mut dirs: Vec<String> = vec!["/usr/local/bin".into(), "/usr/bin".into()];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin").display().to_string());
    }
    let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let path_var = std::env::var_os("PATH");
    let find = |name: &str| {
        locate::locate(&Search {
            env_name: None,
            env_value: None,
            file_name: name,
            beside: None,
            path_var: path_var.as_deref(),
            dirs: &dirs,
        })
    };

    match find("openshell-gateway") {
        Err(_) => out.push(Finding::Fail(format!(
            "openshell-gateway is not installed. Install OpenShell: {OPENSHELL_INSTALL}"
        ))),
        Ok(found) => match run_output(&found.path, "--version")
            .as_deref()
            .and_then(first_version)
        {
            Some((0, 1, patch)) => out.push(Finding::Ok(format!(
                "OpenShell gateway 0.1.{patch} ({})",
                found.path.display()
            ))),
            Some((major, minor, patch)) => out.push(Finding::Fail(format!(
                "OpenShell gateway {major}.{minor}.{patch} is not supported: \
                 the plugin needs a 0.1.x release"
            ))),
            None => out.push(Finding::Warn(format!(
                "cannot read a version from `{} --version`; the plugin needs OpenShell 0.1.x",
                found.path.display()
            ))),
        },
    }
    match find("openshell") {
        Ok(found) => out.push(Finding::Ok(format!(
            "openshell CLI ({})",
            found.path.display()
        ))),
        Err(_) => out.push(Finding::Warn(
            "the openshell CLI is not on PATH (needed to create sandboxes, not by the driver)"
                .to_string(),
        )),
    }

    // The driver that will run this launcher.
    let stock_override = std::env::var_os(STOCK_DRIVER_ENV);
    let driver = locate::locate(&Search {
        env_name: Some(STOCK_DRIVER_ENV),
        env_value: stock_override.as_deref().filter(|value| !value.is_empty()),
        file_name: "openshell-driver-vm",
        beside: launcher.parent(),
        path_var: path_var.as_deref(),
        dirs: &dirs,
    });
    match driver {
        Err(error) => out.push(Finding::Fail(format!(
            "OpenShell's VM driver was not found ({error}). install.sh installs one next to \
             the launcher"
        ))),
        Ok(found) => match run_output(&found.path, "--help") {
            None => out.push(Finding::Fail(format!(
                "cannot run {} --help",
                found.path.display()
            ))),
            Some(help) if help.contains("launcher-bin") => out.push(Finding::Ok(format!(
                "the VM driver {} accepts {LAUNCHER_ENV}",
                found.path.display()
            ))),
            Some(_) => out.push(Finding::Fail(format!(
                "the VM driver {} cannot use a different launcher (no {LAUNCHER_ENV}), so it \
                 would boot libkrun instead. Use the interim driver install.sh installs, or an \
                 OpenShell release that has the option",
                found.path.display()
            ))),
        },
    }

    // The service's environment file is what the driver runs with; the shell only
    // counts when there is no file (a driver started by hand).
    let from_file = driver_env_file_value(LAUNCHER_ENV);
    match from_file.or_else(|| env_text(LAUNCHER_ENV)) {
        None => out.push(Finding::Fail(format!(
            "{LAUNCHER_ENV} is not set in the driver's environment file ({}) or this shell, so the driver would boot libkrun VMs instead of Firecracker. The \
             driver has no default for it: set it in that file",
            driver_env_file().map_or("unknown".to_string(), |p| p.display().to_string())
        ))),
        Some(value) => {
            let configured = PathBuf::from(&value);
            if !configured.is_file() {
                out.push(Finding::Fail(format!(
                    "{LAUNCHER_ENV} is not an existing file: {value}"
                )));
            } else if configured.canonicalize().ok() == launcher.canonicalize().ok() {
                out.push(Finding::Ok(format!(
                    "{LAUNCHER_ENV} points at this launcher"
                )));
            } else {
                out.push(Finding::Warn(format!(
                    "{LAUNCHER_ENV} is {value}, not this launcher ({})",
                    launcher.display()
                )));
            }
        }
    }
}

fn check_host(launcher: &Path, out: &mut Vec<Finding>) {
    out.push(Finding::Heading("Host"));
    let beside = launcher.parent();

    let firecracker = locate::firecracker(beside);
    match &firecracker {
        Ok(found) => {
            out.push(Finding::Ok(format!(
                "firecracker: {} ({})",
                found.path.display(),
                found.source.describe()
            )));
            match check_firecracker_version(&found.path) {
                Ok(version) => out.push(Finding::Ok(version)),
                Err(error) => out.push(Finding::Fail(error)),
            }
        }
        Err(error) => out.push(Finding::Fail(format!(
            "{error}. Install Firecracker (see the README) or set the variable"
        ))),
    }

    let kernel = locate::kernel(beside);
    match &kernel {
        Ok(found) => {
            out.push(Finding::Ok(format!(
                "guest kernel: {} ({})",
                found.path.display(),
                found.source.describe()
            )));
            out.push(check_kernel_file(&found.path));
        }
        Err(error) => out.push(Finding::Fail(format!(
            "{error}. install.sh installs one next to the launcher"
        ))),
    }

    match confine::probe(launcher) {
        Ok(()) => out.push(Finding::Ok(
            "KVM is accessible and the VMM jail can be built".to_string(),
        )),
        Err(error) => {
            let mut text = format!("host cannot run a jailed VMM: {error}");
            if userns_restricted_by_apparmor() && !error.contains("AppArmor") {
                text.push_str(&format!("\n      {}", confine::USERNS_RESTRICTION_HINT));
            }
            out.push(Finding::Fail(text));
        }
    }

    for tool in ["debugfs", "mke2fs"] {
        if tool_present(tool) {
            out.push(Finding::Ok(format!("{tool} is installed")));
        } else {
            out.push(Finding::Fail(format!(
                "{tool} is missing (package e2fsprogs)"
            )));
        }
    }

    let mode = match std::env::var(ENV_SUBUIDS) {
        Ok(value) => value.parse::<SubuidMode>().map_err(|e| e.to_string()),
        Err(_) => Ok(SubuidMode::default()),
    };
    match (mode, &firecracker, &kernel) {
        (Err(error), _, _) => out.push(Finding::Fail(error)),
        (Ok(SubuidMode::Off), _, _) => {
            out.push(Finding::Ok("sub-uid isolation is off".to_string()));
        }
        (Ok(mode), Ok(firecracker), Ok(kernel)) => {
            match subids::preflight(&firecracker.path, &kernel.path) {
                Ok(()) => out.push(Finding::Ok("sub-uid isolation is available".to_string())),
                Err(error) if mode == SubuidMode::Required => out.push(Finding::Fail(format!(
                    "sub-uid isolation is required but unavailable: {error}"
                ))),
                Err(error) => out.push(Finding::Warn(format!(
                    "sub-uid isolation unavailable; VMMs will run as the driver user: {error}"
                ))),
            }
        }
        _ => {}
    }
}

fn check_driver_environment(out: &mut Vec<Finding>) {
    out.push(Finding::Heading("Driver environment"));

    match env_text("OPENSHELL_GRPC_ENDPOINT") {
        None => out.push(Finding::Fail(
            "OPENSHELL_GRPC_ENDPOINT is not set: it tells the host supervisor where the \
             gateway is. Set it in the driver's environment file"
                .to_string(),
        )),
        Some(endpoint) => {
            out.push(Finding::Ok(format!(
                "OPENSHELL_GRPC_ENDPOINT is {endpoint}"
            )));
            if endpoint.starts_with("https://") {
                match env_text("OPENSHELL_VM_TLS_CA") {
                    None => out.push(Finding::Fail(
                        "OPENSHELL_GRPC_ENDPOINT is https:// but OPENSHELL_VM_TLS_CA is not set"
                            .to_string(),
                    )),
                    Some(ca) if Path::new(&ca).is_file() => {
                        out.push(Finding::Ok(format!("TLS CA: {ca}")));
                    }
                    Some(ca) => out.push(Finding::Fail(format!(
                        "OPENSHELL_VM_TLS_CA is not an existing file: {ca}"
                    ))),
                }
            }
        }
    }

    match env_text("OPENSHELL_VM_DRIVER_STATE_DIR") {
        Some(dir) => out.push(Finding::Ok(format!("state directory: {dir}"))),
        None => out.push(Finding::Warn(
            "OPENSHELL_VM_DRIVER_STATE_DIR is not set; the driver will use its own default"
                .to_string(),
        )),
    }

    if env_text("OPENSHELL_VM_BOOTSTRAP_IMAGE").is_some()
        || env_text("OPENSHELL_SANDBOX_IMAGE").is_some()
    {
        out.push(Finding::Ok("a sandbox image is configured".to_string()));
    } else {
        out.push(Finding::Fail(
            "set OPENSHELL_SANDBOX_IMAGE (or OPENSHELL_VM_BOOTSTRAP_IMAGE): the driver needs an \
             image"
                .to_string(),
        ));
    }

    match env_text("OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON") {
        None => out.push(Finding::Fail(
            "OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON is not set; use {} for defaults, matching \
             the gateway"
                .to_string(),
        )),
        Some(json) => match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(_) => out.push(Finding::Ok(
                "OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON is valid JSON (it must also match the \
                 gateway's settings)"
                    .to_string(),
            )),
            Err(error) => out.push(Finding::Fail(format!(
                "OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON is not valid JSON: {error}"
            ))),
        },
    }
}

fn config_base() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

fn driver_env_file() -> Option<PathBuf> {
    Some(
        config_base()?
            .join("openshell")
            .join("firecracker-driver.env"),
    )
}

/// `NAME=value` from the driver's environment file (uncommented, optionally quoted).
fn driver_env_file_value(name: &str) -> Option<String> {
    let text = std::fs::read_to_string(driver_env_file()?).ok()?;
    parse_env_value(&text, name)
}

pub fn parse_env_value(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line
            .trim()
            .strip_prefix(name)?
            .trim_start()
            .strip_prefix('=')?;
        let value = rest.trim().trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Without `gateway_jwt` in gateway.toml the gateway uses the signing keys that
/// OpenShell's installer generates in `<tls dir>/jwt/`.
fn local_jwt_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OPENSHELL_LOCAL_TLS_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join("jwt"));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    Some(state.join("openshell").join("tls").join("jwt"))
}

fn check_local_jwt(out: &mut Vec<Finding>) {
    let Some(dir) = local_jwt_dir() else {
        out.push(Finding::Warn(
            "gateway.toml has no gateway_jwt and the local signing keys cannot be located \
             (HOME is not set)"
                .to_string(),
        ));
        return;
    };
    let files = ["signing.pem", "public.pem", "kid"];
    let present = files.iter().filter(|f| dir.join(f).is_file()).count();
    match present {
        3 => out.push(Finding::Ok(format!(
            "launch authentication: the gateway uses the local signing keys in {}",
            dir.display()
        ))),
        0 => out.push(Finding::Warn(format!(
            "no gateway_jwt in gateway.toml and no local signing keys in {}. The driver \
             requires launch authentication, so sandbox creation fails unless the gateway \
             finds a signing bundle elsewhere",
            dir.display()
        ))),
        _ => out.push(Finding::Fail(format!(
            "partial local signing keys in {}: expected signing.pem, public.pem and kid",
            dir.display()
        ))),
    }
}

fn gateway_config_path() -> Option<PathBuf> {
    Some(config_base()?.join("openshell").join("gateway.toml"))
}

fn check_gateway_config(out: &mut Vec<Finding>) {
    out.push(Finding::Heading("Gateway configuration"));
    let Some(path) = gateway_config_path() else {
        out.push(Finding::Warn(
            "cannot work out where gateway.toml is (HOME is not set)".to_string(),
        ));
        return;
    };
    let text: OsString = match std::fs::read_to_string(&path) {
        Ok(text) => text.into(),
        Err(_) => {
            out.push(Finding::Warn(format!(
                "no {}: if your gateway reads its configuration elsewhere, make sure it sets \
                 compute_driver = \"firecracker\" and [openshell.drivers.firecracker] socket_path",
                path.display()
            )));
            return;
        }
    };
    let scanned = scan_gateway_toml(&text.to_string_lossy());

    match scanned.compute_driver.as_deref() {
        Some("firecracker") => out.push(Finding::Ok(format!(
            "{} selects the firecracker driver",
            path.display()
        ))),
        Some(other) => out.push(Finding::Fail(format!(
            "{} sets compute_driver = \"{other}\", not \"firecracker\"",
            path.display()
        ))),
        None => out.push(Finding::Fail(format!(
            "{} does not set compute_driver = \"firecracker\" under [openshell.gateway]",
            path.display()
        ))),
    }
    match scanned.socket_path {
        Some(socket) => out.push(Finding::Ok(format!("driver socket: {socket}"))),
        None => out.push(Finding::Fail(format!(
            "{} has no socket_path under [openshell.drivers.firecracker]",
            path.display()
        ))),
    }
    if scanned.mentions_jwt {
        out.push(Finding::Ok(
            "gateway.toml configures gateway_jwt for launch authentication".to_string(),
        ));
    } else {
        check_local_jwt(out);
    }
}

/// Run every check and collect the findings.
pub fn run_checks(launcher: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    check_openshell(launcher, &mut findings);
    check_host(launcher, &mut findings);
    check_driver_environment(&mut findings);
    check_gateway_config(&mut findings);
    findings
}

/// Print the report; the process exit code (0 unless something failed).
pub fn run(launcher: &Path) -> i32 {
    let findings = run_checks(launcher);
    let (mut failed, mut warned) = (0_usize, 0_usize);
    for finding in &findings {
        match finding {
            Finding::Heading(text) => println!("\n== {text} =="),
            Finding::Ok(text) => println!("ok    {text}"),
            Finding::Warn(text) => {
                warned += 1;
                println!("warn  {text}");
            }
            Finding::Fail(text) => {
                failed += 1;
                println!("FAIL  {text}");
            }
        }
    }
    println!("\n{failed} failed, {warned} warnings");
    i32::from(failed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_values_are_read_unless_commented() {
        let text = "# OPENSHELL_VM_LAUNCHER_BIN=/no\nOPENSHELL_VM_LAUNCHER_BIN=\"/yes\"\nOTHER=1\n";
        assert_eq!(
            parse_env_value(text, "OPENSHELL_VM_LAUNCHER_BIN").as_deref(),
            Some("/yes")
        );
        assert_eq!(parse_env_value("# X=1\n", "X"), None);
    }

    #[test]
    fn a_missing_firecracker_is_a_failure() {
        assert!(check_firecracker_version(Path::new("/nonexistent/firecracker")).is_err());
    }

    #[test]
    fn versions_are_read_from_typical_output() {
        assert_eq!(first_version("openshell-gateway 0.1.6"), Some((0, 1, 6)));
        assert_eq!(first_version("v0.1.6-pre.1"), Some((0, 1, 6)));
        assert_eq!(
            first_version("openshell 12.34.56 (abc)"),
            Some((12, 34, 56))
        );
        assert_eq!(first_version("no version here"), None);
        assert_eq!(first_version("1.2"), None);
    }

    #[test]
    fn kernels_are_told_apart() {
        let mut elf = vec![0_u8; 0x400];
        elf[..4].copy_from_slice(b"\x7fELF");
        assert_eq!(kernel_kind(&elf), KernelKind::Elf);

        let mut bz = vec![0_u8; 0x400];
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        assert_eq!(kernel_kind(&bz), KernelKind::BzImage);

        let mut arm = vec![0_u8; 0x400];
        arm[0x38..0x3c].copy_from_slice(b"ARMd");
        assert_eq!(kernel_kind(&arm), KernelKind::ArmImage);

        assert_eq!(kernel_kind(&[0x1f, 0x8b, 8, 0]), KernelKind::Gzip);
        assert_eq!(kernel_kind(&[0; 16]), KernelKind::Unknown);
        assert_eq!(kernel_kind(&[]), KernelKind::Unknown);
    }

    #[test]
    fn the_gateway_configuration_is_scanned() {
        let text = r#"
# comment
[openshell]
version = 2

[openshell.gateway]
compute_driver = "firecracker"   # the plugin
gateway_jwt_key = "/x"

[openshell.drivers.firecracker]
socket_path = "/run/user/1000/openshell-firecracker/driver.sock"
"#;
        let scanned = scan_gateway_toml(text);
        assert_eq!(scanned.compute_driver.as_deref(), Some("firecracker"));
        assert_eq!(
            scanned.socket_path.as_deref(),
            Some("/run/user/1000/openshell-firecracker/driver.sock")
        );
        assert!(scanned.mentions_jwt);
    }

    #[test]
    fn keys_in_the_wrong_table_are_ignored() {
        let scanned =
            scan_gateway_toml("[other]\ncompute_driver = \"firecracker\"\nsocket_path = \"/x\"\n");
        assert_eq!(scanned, GatewayToml::default());
    }

    #[test]
    fn an_unconfigured_host_reports_failures_instead_of_panicking() {
        let findings = run_checks(Path::new("/nonexistent/launcher"));
        assert!(findings.iter().any(|f| matches!(f, Finding::Heading(_))));
        assert!(findings.iter().any(|f| matches!(f, Finding::Fail(_))));
    }
}
