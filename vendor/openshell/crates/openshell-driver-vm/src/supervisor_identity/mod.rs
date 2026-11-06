// SPDX-FileCopyrightText: Copyright (c) 2026 openshell-firecracker contributors
// SPDX-License-Identifier: Apache-2.0

//! Supervisor identity: driver-issued SPIFFE JWT-SVIDs for host supervisors
//! (not in upstream `OpenShell`).
//!
//! When enabled, the driver is the SPIFFE JWT-SVID issuer for its sandboxes'
//! supervisors, in place of a SPIRE agent:
//!
//! - Each supervisor is started from a hard link of the supervisor binary in
//!   its sandbox's state directory, and finds a Workload API socket beside it
//!   through `OPENSHELL_PROVIDER_SPIFFE_WORKLOAD_API_SOCKET`, the variable the
//!   stock supervisor already reads for provider token grants.
//! - The socket answers only that supervisor process ([`attest`]) and issues
//!   it `spiffe://<trust domain>/openshell/sandbox/<sandbox id>`.
//! - Relying parties verify tokens with the JWKS served over HTTP ([`jwks`]).
//!
//! Disabled (the default), none of it exists: no HTTP listener, no signing
//! key, no socket or link in a sandbox's state directory, and no variable in
//! the supervisor's environment.

mod attest;
mod jwks;
mod signer;
mod workload_api;

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;
use tracing::{info, warn};

use attest::ExpectedSupervisor;
use signer::Signer;

/// Workload API socket, in the sandbox's state directory.
pub const SOCKET_FILE: &str = "workload-api.sock";
/// Hard link of the supervisor binary, in the sandbox's state directory.
pub const SUPERVISOR_LINK_FILE: &str = "openshell-supervisor";
/// Driver-wide state, under the driver's state directory.
const STATE_DIR: &str = "supervisor-identity";
const SIGNING_KEY_FILE: &str = "jwt-signing-key.p8";
/// `sun_path` holds 108 bytes including the terminating NUL.
const SOCKET_PATH_MAX: usize = 107;
const SPIFFE_ID_PATH_PREFIX: &str = "openshell/sandbox";

pub const DEFAULT_TRUST_DOMAIN: &str = "openshell.local";
pub const DEFAULT_JWKS_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8961);
pub const DEFAULT_SVID_TTL_SECS: u64 = 300;
pub const MIN_SVID_TTL_SECS: u64 = 60;
pub const MAX_SVID_TTL_SECS: u64 = 3600;

/// Operator configuration. Every field but `enabled` is ignored while disabled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SupervisorIdentityConfig {
    pub enabled: bool,
    /// SPIFFE trust domain of issued IDs.
    pub trust_domain: String,
    /// `iss` claim and OIDC issuer: the URL relying parties reach the JWKS
    /// endpoints at. Defaults to `http://<jwks_bind>`.
    pub issuer: Option<String>,
    /// TCP address of the JWKS endpoints.
    pub jwks_bind: SocketAddr,
    /// JWT-SVID lifetime.
    pub svid_ttl_secs: u64,
}

impl Default for SupervisorIdentityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trust_domain: DEFAULT_TRUST_DOMAIN.to_string(),
            issuer: None,
            jwks_bind: DEFAULT_JWKS_BIND,
            svid_ttl_secs: DEFAULT_SVID_TTL_SECS,
        }
    }
}

impl SupervisorIdentityConfig {
    /// Validate an enabled configuration and return its issuer URL.
    pub fn validate(&self) -> Result<String, String> {
        validate_trust_domain(&self.trust_domain)?;
        if !(MIN_SVID_TTL_SECS..=MAX_SVID_TTL_SECS).contains(&self.svid_ttl_secs) {
            return Err(format!(
                "supervisor identity svid_ttl_secs must be between {MIN_SVID_TTL_SECS} and {MAX_SVID_TTL_SECS}"
            ));
        }
        match self.issuer.as_deref() {
            Some(issuer) => validate_issuer(issuer),
            None if self.jwks_bind.ip().is_unspecified() => Err(format!(
                "supervisor identity JWKS listens on {}, which is not an address relying parties can use as the issuer; set the issuer URL",
                self.jwks_bind
            )),
            None => Ok(format!("http://{}", self.jwks_bind)),
        }
    }
}

/// A trust domain is the host part of a SPIFFE ID: lowercase letters, digits,
/// `.`, `-` and `_`.
fn validate_trust_domain(trust_domain: &str) -> Result<(), String> {
    if trust_domain.is_empty()
        || trust_domain.len() > 255
        || !trust_domain.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(format!(
            "supervisor identity trust domain '{trust_domain}' must be 1 to 255 characters of [a-z0-9._-]"
        ));
    }
    Ok(())
}

/// An OIDC issuer is an http(s) URL without query or fragment. A trailing `/`
/// is dropped so `iss` and the discovery document agree.
fn validate_issuer(issuer: &str) -> Result<String, String> {
    let url = url::Url::parse(issuer)
        .map_err(|error| format!("supervisor identity issuer '{issuer}' is not a URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(format!(
            "supervisor identity issuer '{issuer}' must be an http:// or https:// URL with a host"
        ));
    }
    if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() {
        return Err(format!(
            "supervisor identity issuer '{issuer}' must not have credentials, a query or a fragment"
        ));
    }
    Ok(issuer.trim_end_matches('/').to_string())
}

/// The driver's issuer: configuration and signing key.
#[derive(Debug)]
pub struct SupervisorIdentity {
    config: SupervisorIdentityConfig,
    issuer: String,
    signer: Signer,
}

impl SupervisorIdentity {
    /// Validate `config` and load (or create) the signing key under
    /// `driver_state_dir`. `sample_socket` is the socket path of a sandbox
    /// with a typical id, checked against the socket path limit so a state
    /// directory that is too deep fails at startup rather than per sandbox.
    pub fn load(
        config: &SupervisorIdentityConfig,
        driver_state_dir: &Path,
        sample_socket: &Path,
    ) -> Result<Arc<Self>, String> {
        let issuer = config.validate()?;
        check_socket_path(sample_socket)?;
        let directory = driver_state_dir.join(STATE_DIR);
        fs::create_dir_all(&directory)
            .and_then(|()| fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)))
            .map_err(|error| format!("create {}: {error}", directory.display()))?;
        let signer = Signer::load_or_create(&directory.join(SIGNING_KEY_FILE))?;
        info!(
            issuer = %issuer,
            trust_domain = %config.trust_domain,
            kid = %signer.kid(),
            "supervisor identity enabled"
        );
        Ok(Arc::new(Self {
            config: config.clone(),
            issuer,
            signer,
        }))
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn trust_domain(&self) -> &str {
        &self.config.trust_domain
    }

    pub fn jwks_bind(&self) -> SocketAddr {
        self.config.jwks_bind
    }

    pub fn spiffe_id(&self, sandbox_id: &str) -> String {
        format!(
            "spiffe://{}/{SPIFFE_ID_PATH_PREFIX}/{sandbox_id}",
            self.config.trust_domain
        )
    }

    /// Sign a JWT-SVID for `sandbox_id`'s supervisor.
    fn mint(&self, sandbox_id: &str, audience: &[String]) -> Result<String, String> {
        let now = unix_now()?;
        let jti = openshell_crypto::random_bytes::<16>()
            .map_err(|error| format!("generate token id: {error}"))?;
        // One audience is a string, as SPIRE (go-jose) writes it; several
        // are an array.
        let aud = match audience {
            [one] => serde_json::Value::String(one.clone()),
            many => serde_json::json!(many),
        };
        self.signer.sign(&serde_json::json!({
            "iss": self.issuer,
            "sub": self.spiffe_id(sandbox_id),
            "aud": aud,
            "iat": now,
            "exp": now + self.config.svid_ttl_secs,
            "jti": hex(&jti),
        }))
    }

    /// Verify a JWT-SVID issued by this driver for `audience`, as
    /// `ValidateJWTSVID` does. Returns the SPIFFE ID and the claims.
    fn validate(
        &self,
        token: &str,
        audience: &str,
    ) -> Result<(String, serde_json::Map<String, serde_json::Value>), String> {
        let claims = self.signer.verify(token)?;
        if claims.get("iss").and_then(serde_json::Value::as_str) != Some(self.issuer.as_str()) {
            return Err("JWT-SVID has an unexpected issuer".to_string());
        }
        let subject = claims
            .get("sub")
            .and_then(serde_json::Value::as_str)
            .filter(|subject| {
                subject
                    .strip_prefix("spiffe://")
                    .and_then(|rest| rest.strip_prefix(self.config.trust_domain.as_str()))
                    .is_some_and(|path| path.starts_with('/'))
            })
            .ok_or_else(|| "JWT-SVID subject is not in this trust domain".to_string())?
            .to_string();
        let expiry = claims
            .get("exp")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "JWT-SVID has no expiry".to_string())?;
        if expiry <= unix_now()? {
            return Err("JWT-SVID has expired".to_string());
        }
        let audience_matches = match claims.get("aud") {
            Some(serde_json::Value::String(value)) => value == audience,
            Some(serde_json::Value::Array(values)) => {
                values.iter().any(|value| value.as_str() == Some(audience))
            }
            _ => false,
        };
        if !audience_matches {
            return Err("JWT-SVID audience does not match".to_string());
        }
        Ok((subject, claims))
    }

    /// The JWKS relying parties verify with (`use: sig`).
    pub fn oidc_jwks(&self) -> serde_json::Value {
        serde_json::json!({ "keys": [self.signer.public_jwk("sig")] })
    }

    /// The trust domain's JWT bundle for `FetchJWTBundles`: a JWKS whose keys
    /// are marked `use: jwt-svid`, as the SPIFFE bundle format requires.
    fn spiffe_bundle(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&serde_json::json!({
            "keys": [self.signer.public_jwk("jwt-svid")]
        }))
        .map_err(|error| error.to_string())
    }

    pub fn discovery_document(&self) -> serde_json::Value {
        serde_json::json!({
            "issuer": self.issuer,
            "jwks_uri": format!("{}{}", self.issuer, jwks::JWKS_PATH),
            "authorization_endpoint": "",
            "response_types_supported": ["id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": [signer::JWT_ALGORITHM],
        })
    }

    /// Serve the JWKS endpoints on `listener` until `shutdown` completes.
    pub async fn serve_jwks(
        self: Arc<Self>,
        listener: tokio::net::TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        axum::serve(listener, jwks::router(self))
            .with_graceful_shutdown(shutdown)
            .await
    }

    /// Set up a sandbox's socket and supervisor link before the supervisor
    /// starts. Anything a previous launch left behind is replaced.
    pub fn prepare_sandbox(
        &self,
        sandbox_state_dir: &Path,
        supervisor_binary: &Path,
    ) -> Result<PreparedSandbox, String> {
        remove_sandbox_artifacts(sandbox_state_dir);
        let paths = SandboxPaths::new(sandbox_state_dir);
        check_socket_path(&paths.socket)?;
        link_supervisor(supervisor_binary, &paths.supervisor)?;
        let listener = std::os::unix::net::UnixListener::bind(&paths.socket)
            .and_then(|listener| {
                fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
                listener.set_nonblocking(true)?;
                Ok(listener)
            })
            .map_err(|error| {
                remove_sandbox_artifacts(sandbox_state_dir);
                format!(
                    "bind workload API socket {}: {error}",
                    paths.socket.display()
                )
            })?;
        Ok(PreparedSandbox {
            paths: Some(paths),
            listener: Some(listener),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SandboxPaths {
    socket: PathBuf,
    supervisor: PathBuf,
}

impl SandboxPaths {
    fn new(sandbox_state_dir: &Path) -> Self {
        Self {
            socket: sandbox_state_dir.join(SOCKET_FILE),
            supervisor: sandbox_state_dir.join(SUPERVISOR_LINK_FILE),
        }
    }

    fn remove(&self) {
        for path in [&self.socket, &self.supervisor] {
            if let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                warn!(path = %path.display(), %error, "remove supervisor identity file");
            }
        }
    }
}

/// Remove a sandbox's socket and supervisor link, if present. Run on every
/// launch, enabled or not, so a sandbox last started with supervisor identity
/// enabled keeps nothing of it once it is disabled.
pub fn remove_sandbox_artifacts(sandbox_state_dir: &Path) {
    SandboxPaths::new(sandbox_state_dir).remove();
}

/// Socket bound and link made; the supervisor has not started.
#[derive(Debug)]
pub struct PreparedSandbox {
    paths: Option<SandboxPaths>,
    listener: Option<std::os::unix::net::UnixListener>,
}

impl PreparedSandbox {
    /// The executable the supervisor must be started from.
    pub fn supervisor_path(&self) -> &Path {
        &self
            .paths
            .as_ref()
            .expect("paths present until started")
            .supervisor
    }

    /// The value for `OPENSHELL_PROVIDER_SPIFFE_WORKLOAD_API_SOCKET`.
    pub fn socket_path(&self) -> &Path {
        &self
            .paths
            .as_ref()
            .expect("paths present until started")
            .socket
    }

    /// Begin serving the supervisor the driver started as `supervisor_pid`
    /// from [`Self::supervisor_path`]. Connections made before this are held
    /// in the listen backlog and answered once its identity is recorded.
    pub fn start(
        mut self,
        identity: Arc<SupervisorIdentity>,
        sandbox_id: &str,
        supervisor_pid: u32,
    ) -> Result<SandboxEndpoint, String> {
        let paths = self.paths.take().expect("paths present until started");
        let listener = self
            .listener
            .take()
            .expect("listener present until started");
        let started = (|| {
            let expected = ExpectedSupervisor::capture(supervisor_pid, &paths.supervisor)?;
            let listener = tokio::net::UnixListener::from_std(listener)
                .map_err(|error| format!("register workload API socket: {error}"))?;
            Ok(
                workload_api::WorkloadApi::new(identity, sandbox_id.to_string(), expected)
                    .spawn(listener),
            )
        })();
        match started {
            Ok(task) => Ok(SandboxEndpoint { task, paths }),
            Err(error) => {
                paths.remove();
                Err(error)
            }
        }
    }
}

impl Drop for PreparedSandbox {
    fn drop(&mut self) {
        if let Some(paths) = self.paths.take() {
            drop(self.listener.take());
            paths.remove();
        }
    }
}

/// A sandbox's running Workload API. Dropping it stops the server and removes
/// the socket and the supervisor link.
#[derive(Debug)]
pub struct SandboxEndpoint {
    task: JoinHandle<()>,
    paths: SandboxPaths,
}

impl SandboxEndpoint {
    pub fn socket_path(&self) -> &Path {
        &self.paths.socket
    }
}

impl Drop for SandboxEndpoint {
    fn drop(&mut self) {
        self.task.abort();
        self.paths.remove();
    }
}

pub fn check_socket_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().len() > SOCKET_PATH_MAX {
        return Err(format!(
            "workload API socket path {} is {} bytes, more than the {SOCKET_PATH_MAX} a UNIX socket allows; use a shorter driver state directory",
            path.display(),
            path.as_os_str().len()
        ));
    }
    Ok(())
}

/// Hard-link the supervisor binary to `link`. The link is what the
/// supervisor runs, so `/proc/<pid>/exe` names this sandbox. Where a hard
/// link is impossible (another filesystem, or `fs.protected_hardlinks` and a
/// binary owned by someone else), a private copy serves the same purpose.
fn link_supervisor(source: &Path, link: &Path) -> Result<(), String> {
    let metadata = fs::metadata(source)
        .map_err(|error| format!("inspect supervisor binary {}: {error}", source.display()))?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(format!(
            "supervisor binary {} is not an executable file",
            source.display()
        ));
    }
    match fs::hard_link(source, link) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EXDEV | libc::EPERM | libc::EMLINK)
            ) =>
        {
            warn!(
                source = %source.display(),
                %error,
                "cannot hard-link the supervisor binary; copying it instead"
            );
            fs::copy(source, link)
                .and_then(|_| fs::set_permissions(link, fs::Permissions::from_mode(0o700)))
                .map_err(|error| {
                    let _ = fs::remove_file(link);
                    format!("copy supervisor binary to {}: {error}", link.display())
                })
        }
        Err(error) => Err(format!(
            "hard-link supervisor binary to {}: {error}",
            link.display()
        )),
    }
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| "system clock is before 1970".to_string())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests;
