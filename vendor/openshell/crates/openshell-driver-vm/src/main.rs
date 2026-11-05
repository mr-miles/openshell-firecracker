// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use futures::Stream;
use miette::{IntoDiagnostic, Result};
use openshell_core::proto::compute::v1::compute_driver_server::ComputeDriverServer;
use openshell_core::sandbox_env::SupervisorOtlpArgs;
use openshell_driver_vm::VERSION;
use openshell_driver_vm::{VmDriver, VmDriverConfig, procguard};
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::net::{UnixListener, UnixStream};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "openshell-driver-vm")]
#[command(version = VERSION)]
#[allow(clippy::struct_excessive_bools)]
struct Args {
    /// VM launcher executable the driver runs for every VM (required). The
    /// launcher receives the driver's `--internal-run-vm` flags.
    #[arg(long, env = "OPENSHELL_VM_LAUNCHER_BIN")]
    launcher_bin: Option<PathBuf>,

    /// Operator-owned JSON policy; omitted means driver config disabled and labels required.
    #[arg(
        long,
        env = "OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON",
        default_value = "{}"
    )]
    admission_config_json: openshell_core::resource_admission::DriverAdmissionConfig,
    #[arg(long, hide = true)]
    internal_prepare_image: Option<PathBuf>,

    #[arg(long, env = "OPENSHELL_COMPUTE_DRIVER_BIND")]
    bind_address: Option<SocketAddr>,

    #[arg(long, env = "OPENSHELL_COMPUTE_DRIVER_SOCKET")]
    bind_socket: Option<PathBuf>,

    #[arg(long, hide = true)]
    expected_peer_pid: Option<u32>,

    #[arg(
        long,
        env = "OPENSHELL_COMPUTE_DRIVER_ALLOW_UNAUTHENTICATED_TCP",
        default_value_t = false
    )]
    allow_unauthenticated_tcp: bool,

    #[arg(
        long,
        env = "OPENSHELL_COMPUTE_DRIVER_ALLOW_SAME_UID_PEER",
        default_value_t = false
    )]
    allow_same_uid_peer: bool,

    #[arg(long, env = "OPENSHELL_LOG_LEVEL", default_value = "info")]
    log_level: String,

    #[command(flatten)]
    otlp: SupervisorOtlpArgs,

    #[arg(long, env = "OPENSHELL_GATEWAY_NAME")]
    gateway_name: Option<String>,

    #[arg(long = "grpc-endpoint", env = "OPENSHELL_GRPC_ENDPOINT")]
    grpc_endpoint: Option<String>,

    #[arg(long, env = "OPENSHELL_SANDBOX_IMAGE", default_value = "")]
    default_image: String,

    #[arg(long, env = "OPENSHELL_VM_BOOTSTRAP_IMAGE", default_value = "")]
    bootstrap_image: String,

    #[arg(
        long,
        env = "OPENSHELL_VM_DRIVER_STATE_DIR",
        default_value = "target/openshell-vm-driver"
    )]
    state_dir: PathBuf,

    #[arg(long = "guest-tls-ca", env = "OPENSHELL_VM_TLS_CA")]
    guest_tls_ca: Option<PathBuf>,

    /// Deprecated; client certificates are rejected.
    #[arg(long = "guest-tls-cert", env = "OPENSHELL_VM_TLS_CERT")]
    guest_tls_cert: Option<PathBuf>,

    /// Deprecated; client private keys are rejected.
    #[arg(long = "guest-tls-key", env = "OPENSHELL_VM_TLS_KEY")]
    guest_tls_key: Option<PathBuf>,

    /// Gateway-owned normalized destination trust artifact. This hidden option
    /// is supplied only by the in-process gateway VM factory.
    #[arg(long, hide = true)]
    additional_ca_bundle: Option<PathBuf>,

    /// Expected SHA-256 generation of the gateway-owned destination trust
    /// artifact. Supplied with the path so the child can authenticate its
    /// one-time startup read without receiving CA bytes on argv or in env.
    #[arg(long, hide = true)]
    additional_ca_digest: Option<String>,

    /// Expected certificate count in the gateway-normalized destination trust
    /// artifact.
    #[arg(long, hide = true)]
    additional_ca_certificate_count: Option<usize>,

    /// Corporate forward proxy for supervisor TLS egress.
    #[arg(long, env = "OPENSHELL_VM_UPSTREAM_PROXY")]
    upstream_proxy: Option<String>,

    #[arg(long, env = "OPENSHELL_VM_UPSTREAM_NO_PROXY")]
    upstream_no_proxy: Option<String>,

    /// Root-owned gateway-host file containing `user:pass` proxy credentials.
    #[arg(long, env = "OPENSHELL_VM_UPSTREAM_PROXY_AUTH_FILE")]
    upstream_proxy_auth_file: Option<PathBuf>,

    /// Explicitly acknowledge cleartext Basic authentication to an http proxy.
    #[arg(
        long,
        env = "OPENSHELL_VM_UPSTREAM_PROXY_AUTH_ALLOW_INSECURE",
        default_value_t = false
    )]
    upstream_proxy_auth_allow_insecure: bool,

    #[arg(
        long,
        env = "OPENSHELL_VM_UPSTREAM_PROXY_CONNECT_BY_HOSTNAME",
        default_value_t = false
    )]
    upstream_proxy_connect_by_hostname: bool,

    /// Gateway-host PEM CA bundle trusted for the corporate proxy and for
    /// server certificates re-signed by a TLS-intercepting proxy.
    #[arg(long, env = "OPENSHELL_VM_UPSTREAM_PROXY_CA_BUNDLE")]
    upstream_proxy_ca_bundle: Option<PathBuf>,

    /// Guest-reachable SPIFFE Workload API endpoint (`tcp:IP:port`).
    #[arg(
        long = "provider-spiffe-workload-api-tcp-endpoint",
        env = "OPENSHELL_PROVIDER_SPIFFE_WORKLOAD_API_TCP_ENDPOINT"
    )]
    provider_spiffe_workload_api_tcp_endpoint: Option<String>,

    /// Explicit acknowledgement that the configured Workload API listener is exposed to VM guests.
    #[arg(
        long,
        env = "OPENSHELL_PROVIDER_SPIFFE_ALLOW_GUEST_TCP",
        default_value_t = false
    )]
    provider_spiffe_allow_guest_tcp: bool,

    #[arg(long, env = "OPENSHELL_VM_DRIVER_VCPUS", default_value_t = 2)]
    vcpus: u8,

    #[arg(long, env = "OPENSHELL_VM_DRIVER_MEM_MIB", default_value_t = 2048)]
    mem_mib: u32,

    #[arg(long, env = "OPENSHELL_VM_OVERLAY_DISK_MIB", default_value_t = 4096)]
    overlay_disk_mib: u64,

    #[arg(long, env = "OPENSHELL_VM_SANDBOX_UID")]
    sandbox_uid: Option<u32>,

    #[arg(long, env = "OPENSHELL_VM_SANDBOX_GID")]
    sandbox_gid: Option<u32>,

    // Corporate forward proxy for sandbox egress. Operator-owned: these reach
    // the host supervisor on its argv, which the sandbox image and the
    // user-supplied environment cannot influence.
    #[arg(long, env = "OPENSHELL_VM_HTTPS_PROXY")]
    https_proxy: Option<String>,

    #[arg(long, env = "OPENSHELL_VM_NO_PROXY")]
    no_proxy: Option<String>,

    #[arg(long, env = "OPENSHELL_VM_PROXY_AUTH_FILE")]
    proxy_auth_file: Option<String>,

    // Value-taking rather than a presence flag so an explicit `false` in
    // `[openshell.drivers.vm]` survives the gateway -> driver hop and still
    // trips the "acknowledgement without a credential" check.
    #[arg(long, env = "OPENSHELL_VM_PROXY_AUTH_ALLOW_INSECURE")]
    proxy_auth_allow_insecure: Option<bool>,

    #[arg(long, env = "OPENSHELL_VM_PROXY_CONNECT_BY_HOSTNAME")]
    proxy_connect_by_hostname: Option<bool>,

    #[arg(long, env = "OPENSHELL_VM_PROXY_CA_BUNDLE")]
    proxy_ca_bundle: Option<String>,

    #[arg(long, env = "OPENSHELL_VM_ROOTFS_TAR_STAGING_DIR")]
    rootfs_tar_staging_dir: Option<PathBuf>,

    #[arg(long, env = "OPENSHELL_VM_ROOTFS_TAR_MAX_BYTES")]
    rootfs_tar_max_bytes: Option<u64>,

    /// Working directory for a sandbox whose image sets none. Unset: `/`.
    #[arg(long, env = "OPENSHELL_VM_DEFAULT_WORKDIR")]
    default_workdir: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(request) = args.internal_prepare_image {
        openshell_driver_vm::driver::run_image_preparation_worker(&request)
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        return Ok(());
    }
    let _tracing = openshell_otel::install_driver_tracing(
        openshell_driver_vm::otel_tracing::TRACING,
        openshell_otel::DriverTracingConfig {
            endpoint: args.otlp.endpoint.as_deref(),
            protocol: args.otlp.protocol.into(),
            ca_file: args.otlp.ca_file.as_deref(),
            cert_file: args.otlp.cert_file.as_deref(),
            key_file: args.otlp.key_file.as_deref(),
            gateway_name: args.gateway_name.as_deref(),
            service_version: VERSION,
            log_level: &args.log_level,
        },
    );
    args.otlp.warn_deprecated();

    let listen_mode = compute_driver_listen_mode(&args).map_err(|err| miette::miette!("{err}"))?;

    // Arm procguard so that if the gateway is killed (SIGKILL or crash)
    // we also die. Without this the driver is reparented to init and
    // keeps its per-sandbox VM launchers alive forever.
    if let Err(err) = procguard::die_with_parent() {
        tracing::warn!(
            error = %err,
            "procguard arm failed; gateway crashes may orphan this driver"
        );
    }

    let supervisor_otlp = args.otlp.supervisor_otlp();
    let driver = VmDriver::new(VmDriverConfig {
        allow_driver_config: args.admission_config_json.allow_driver_config,
        resource_admission: args.admission_config_json.resource_admission.clone(),
        grpc_endpoint: args
            .grpc_endpoint
            .ok_or_else(|| miette::miette!("OPENSHELL_GRPC_ENDPOINT is required"))?,
        supervisor_otlp,
        supervisor_gateway_name: args.gateway_name.clone(),
        state_dir: args.state_dir.clone(),
        launcher_bin: args.launcher_bin.clone(),
        default_image: args.default_image.clone(),
        bootstrap_image: args.bootstrap_image.clone(),
        log_level: args.log_level.clone(),
        vcpus: args.vcpus,
        mem_mib: args.mem_mib,
        overlay_disk_mib: args.overlay_disk_mib,
        guest_tls_ca: args.guest_tls_ca.clone(),
        guest_tls_cert: args.guest_tls_cert.clone(),
        guest_tls_key: args.guest_tls_key.clone(),
        additional_ca_bundle: args.additional_ca_bundle.clone(),
        additional_ca_digest: args.additional_ca_digest.clone(),
        additional_ca_certificate_count: args.additional_ca_certificate_count,
        additional_ca_snapshot: None,
        upstream_proxy: openshell_core::UpstreamProxyConfig {
            https_proxy: args.upstream_proxy.clone(),
            no_proxy: args.upstream_no_proxy.clone(),
            proxy_auth_file: args.upstream_proxy_auth_file.clone(),
            proxy_auth_allow_insecure: args.upstream_proxy_auth_allow_insecure.then_some(true),
            proxy_connect_by_hostname: args.upstream_proxy_connect_by_hostname.then_some(true),
        },
        proxy_ca_bundle: args.upstream_proxy_ca_bundle.clone(),
        provider_spiffe_workload_api_tcp_endpoint: args
            .provider_spiffe_workload_api_tcp_endpoint
            .clone(),
        provider_spiffe_allow_guest_tcp: args.provider_spiffe_allow_guest_tcp,
        sandbox_uid: args.sandbox_uid,
        sandbox_gid: args.sandbox_gid,
        rootfs_tar_staging_dir: args.rootfs_tar_staging_dir.clone(),
        rootfs_tar_max_bytes: args.rootfs_tar_max_bytes,
        default_workdir: args.default_workdir.clone(),
    })
    .await
    .map_err(|err| miette::miette!("{err}"))?;

    let socket_cleanup = driver.clone();
    let result = match listen_mode {
        ComputeDriverListenMode::Unix {
            socket_path,
            expected_peer_pid,
        } => {
            prepare_compute_driver_socket(&socket_path).map_err(|err| miette::miette!("{err}"))?;

            info!(socket = %socket_path.display(), "Starting vm compute driver");
            let listener = UnixListener::bind(&socket_path).into_diagnostic()?;
            restrict_socket_permissions(&socket_path).map_err(|err| miette::miette!("{err}"))?;
            let result = tonic::transport::Server::builder()
                .layer(openshell_otel::compute_driver_rpc_layer())
                .add_service(ComputeDriverServer::new(driver))
                .serve_with_incoming_shutdown(
                    AuthenticatedUnixIncoming::new(listener, expected_peer_pid),
                    shutdown_signal(),
                )
                .await
                .into_diagnostic();
            let _ = std::fs::remove_file(&socket_path);
            result
        }
        ComputeDriverListenMode::Tcp(bind_address) => {
            info!(address = %bind_address, "Starting unauthenticated dev vm compute driver");
            tonic::transport::Server::builder()
                .layer(openshell_otel::compute_driver_rpc_layer())
                .add_service(ComputeDriverServer::new(driver))
                .serve_with_shutdown(bind_address, shutdown_signal())
                .await
                .into_diagnostic()
        }
    };
    socket_cleanup.remove_socket_root();
    result
}

async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::warn!(%error, "failed to listen for Ctrl-C");
            }
        }
        _ = terminate.recv() => {}
    }
    info!("Shutdown signal received; stopping vm compute driver");
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComputeDriverListenMode {
    Unix {
        socket_path: PathBuf,
        expected_peer_pid: Option<u32>,
    },
    Tcp(SocketAddr),
}

fn compute_driver_listen_mode(args: &Args) -> std::result::Result<ComputeDriverListenMode, String> {
    if let Some(socket_path) = args.bind_socket.clone() {
        if args.expected_peer_pid.is_none() && !args.allow_same_uid_peer {
            return Err(
                "--expected-peer-pid is required with --bind-socket; use --allow-same-uid-peer only for local development"
                    .to_string(),
            );
        }
        return Ok(ComputeDriverListenMode::Unix {
            socket_path,
            expected_peer_pid: args.expected_peer_pid,
        });
    }

    if !args.allow_unauthenticated_tcp {
        return Err(
            "--bind-socket is required; unauthenticated TCP mode is disabled unless --allow-unauthenticated-tcp is set for local development"
                .to_string(),
        );
    }

    let Some(bind_address) = args.bind_address else {
        return Err("--bind-address is required with --allow-unauthenticated-tcp".to_string());
    };

    Ok(ComputeDriverListenMode::Tcp(bind_address))
}

fn prepare_compute_driver_socket(socket_path: &Path) -> std::result::Result<(), String> {
    let Some(parent) = socket_path.parent() else {
        return Err(format!(
            "vm compute driver socket path '{}' has no parent directory",
            socket_path.display()
        ));
    };
    let expected_uid = current_euid();
    prepare_private_socket_dir(parent, expected_uid)?;
    remove_stale_socket(socket_path, expected_uid)
}

fn current_euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn prepare_private_socket_dir(
    socket_dir: &Path,
    expected_uid: u32,
) -> std::result::Result<(), String> {
    std::fs::create_dir_all(socket_dir)
        .map_err(|err| format!("create socket dir {}: {err}", socket_dir.display()))?;
    let metadata = std::fs::symlink_metadata(socket_dir)
        .map_err(|err| format!("stat socket dir {}: {err}", socket_dir.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(format!(
            "socket dir {} is a symlink; refusing to use it",
            socket_dir.display()
        ));
    }
    if !file_type.is_dir() {
        return Err(format!(
            "socket dir {} is not a directory",
            socket_dir.display()
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(format!(
            "socket dir {} is owned by uid {} but current euid is {}",
            socket_dir.display(),
            metadata.uid(),
            expected_uid
        ));
    }
    std::fs::set_permissions(socket_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("chmod socket dir {}: {err}", socket_dir.display()))
}

fn remove_stale_socket(socket_path: &Path, expected_uid: u32) -> std::result::Result<(), String> {
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("stat socket {}: {err}", socket_path.display())),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(format!(
            "socket {} is a symlink; refusing to remove it",
            socket_path.display()
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(format!(
            "socket {} is owned by uid {} but current euid is {}",
            socket_path.display(),
            metadata.uid(),
            expected_uid
        ));
    }
    if !file_type.is_socket() {
        return Err(format!(
            "socket path {} exists but is not a Unix socket",
            socket_path.display()
        ));
    }
    std::fs::remove_file(socket_path)
        .map_err(|err| format!("remove stale socket {}: {err}", socket_path.display()))
}

fn restrict_socket_permissions(socket_path: &Path) -> std::result::Result<(), String> {
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("chmod socket {}: {err}", socket_path.display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerCredentials {
    uid: u32,
    pid: Option<i32>,
}

fn peer_credentials(stream: &UnixStream) -> std::result::Result<PeerCredentials, String> {
    let credentials = stream
        .peer_cred()
        .map_err(|err| format!("read peer credentials: {err}"))?;
    Ok(PeerCredentials {
        uid: credentials.uid(),
        pid: credentials.pid(),
    })
}

fn authorize_peer_credentials(
    peer: PeerCredentials,
    driver_uid: u32,
    gateway_pid: Option<u32>,
) -> std::result::Result<(), String> {
    if peer.uid != driver_uid {
        return Err(format!(
            "peer uid {} does not match current euid {}",
            peer.uid, driver_uid
        ));
    }
    let Some(gateway_pid) = gateway_pid else {
        return Ok(());
    };
    let Some(peer_process_id) = peer.pid.and_then(|pid| u32::try_from(pid).ok()) else {
        return Err(format!(
            "peer pid is unavailable; expected gateway pid {gateway_pid}"
        ));
    };
    if peer_process_id != gateway_pid {
        return Err(format!(
            "peer pid {peer_process_id} does not match expected gateway pid {gateway_pid}"
        ));
    }
    Ok(())
}

struct AuthenticatedUnixIncoming {
    listener: UnixListener,
    expected_uid: u32,
    expected_peer_pid: Option<u32>,
}

impl AuthenticatedUnixIncoming {
    fn new(listener: UnixListener, expected_peer_pid: Option<u32>) -> Self {
        Self {
            listener,
            expected_uid: current_euid(),
            expected_peer_pid,
        }
    }
}

impl Stream for AuthenticatedUnixIncoming {
    type Item = io::Result<UnixStream>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.listener.poll_accept(cx) {
                Poll::Ready(Ok((stream, _addr))) => {
                    let authorized = peer_credentials(&stream).and_then(|peer| {
                        authorize_peer_credentials(peer, this.expected_uid, this.expected_peer_pid)
                    });
                    match authorized {
                        Ok(()) => return Poll::Ready(Some(Ok(stream))),
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "rejected vm compute driver UDS client"
                            );
                        }
                    }
                }
                Poll::Ready(Err(err)) => return Poll::Ready(Some(Err(err))),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Args, ComputeDriverListenMode, PeerCredentials, authorize_peer_credentials,
        compute_driver_listen_mode,
    };
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn corporate_proxy_flags_parse_into_driver_settings() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--upstream-proxy",
            "http://proxy.corp.com:8080",
            "--upstream-no-proxy",
            "10.0.0.0/8,.svc.cluster.local",
            "--upstream-proxy-auth-file",
            "/etc/openshell/secrets/proxy-auth",
            "--upstream-proxy-auth-allow-insecure",
            "--upstream-proxy-connect-by-hostname",
            "--upstream-proxy-ca-bundle",
            "/etc/openshell/tls/proxy-ca.pem",
        ]);

        assert_eq!(
            args.upstream_proxy.as_deref(),
            Some("http://proxy.corp.com:8080")
        );
        assert_eq!(
            args.upstream_no_proxy.as_deref(),
            Some("10.0.0.0/8,.svc.cluster.local")
        );
        assert_eq!(
            args.upstream_proxy_auth_file.as_deref(),
            Some(PathBuf::from("/etc/openshell/secrets/proxy-auth").as_path())
        );
        assert!(args.upstream_proxy_auth_allow_insecure);
        assert!(args.upstream_proxy_connect_by_hostname);
        assert_eq!(
            args.upstream_proxy_ca_bundle.as_deref(),
            Some(PathBuf::from("/etc/openshell/tls/proxy-ca.pem").as_path())
        );
    }

    #[test]
    fn corporate_proxy_settings_default_to_unset() {
        let args = Args::parse_from(["openshell-driver-vm"]);
        assert!(args.upstream_proxy.is_none());
        assert!(args.upstream_no_proxy.is_none());
        assert!(args.upstream_proxy_auth_file.is_none());
        assert!(!args.upstream_proxy_auth_allow_insecure);
        assert!(!args.upstream_proxy_connect_by_hostname);
        assert!(args.upstream_proxy_ca_bundle.is_none());
    }

    #[test]
    fn internal_network_trust_options_have_no_environment_aliases() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--additional-ca-bundle",
            "/var/lib/openshell/network/additional-ca.crt",
            "--additional-ca-digest",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--additional-ca-certificate-count",
            "2",
        ]);
        assert_eq!(
            args.additional_ca_bundle.as_deref(),
            Some(PathBuf::from("/var/lib/openshell/network/additional-ca.crt").as_path())
        );
        assert_eq!(
            args.additional_ca_digest.as_deref(),
            Some("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(args.additional_ca_certificate_count, Some(2));

        temp_env::with_vars(
            [
                (
                    "OPENSHELL_VM_ADDITIONAL_CA_BUNDLE",
                    Some("/tmp/untrusted.crt"),
                ),
                ("OPENSHELL_VM_ADDITIONAL_CA_DIGEST", Some("sha256:bad")),
                ("OPENSHELL_VM_ADDITIONAL_CA_CERTIFICATE_COUNT", Some("999")),
            ],
            || {
                let args = Args::parse_from(["openshell-driver-vm"]);
                assert!(args.additional_ca_bundle.is_none());
                assert!(args.additional_ca_digest.is_none());
                assert!(args.additional_ca_certificate_count.is_none());
            },
        );
    }

    #[test]
    fn peer_authorization_accepts_matching_uid_and_pid() {
        authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: Some(42),
            },
            1000,
            Some(42),
        )
        .unwrap();
    }

    #[test]
    fn peer_authorization_rejects_wrong_pid() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: Some(7),
            },
            1000,
            Some(42),
        )
        .expect_err("wrong pid should be rejected");
        assert!(err.contains("does not match expected gateway pid"));
    }

    #[test]
    fn peer_authorization_rejects_wrong_uid() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1001,
                pid: Some(42),
            },
            1000,
            Some(42),
        )
        .expect_err("wrong uid should be rejected");
        assert!(err.contains("does not match current euid"));
    }

    #[test]
    fn peer_authorization_rejects_missing_pid_when_expected() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: None,
            },
            1000,
            Some(42),
        )
        .expect_err("missing pid should be rejected");
        assert!(err.contains("peer pid is unavailable"));
    }

    #[test]
    fn peer_authorization_accepts_matching_uid_without_expected_pid() {
        authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: None,
            },
            1000,
            None,
        )
        .unwrap();
    }

    #[test]
    fn listen_mode_rejects_default_tcp() {
        let args = Args::parse_from(["openshell-driver-vm"]);
        let err = compute_driver_listen_mode(&args).expect_err("default TCP should be disabled");
        assert!(err.contains("--bind-socket is required"));
    }

    #[test]
    fn accepts_canonical_grpc_endpoint_flag() {
        let args = Args::try_parse_from([
            "openshell-driver-vm",
            "--grpc-endpoint",
            "http://127.0.0.1:8080",
        ])
        .unwrap();
        assert_eq!(args.grpc_endpoint.as_deref(), Some("http://127.0.0.1:8080"));
    }

    #[test]
    fn rejects_legacy_openshell_endpoint_flag() {
        let error = Args::try_parse_from([
            "openshell-driver-vm",
            "--openshell-endpoint",
            "http://127.0.0.1:8080",
        ])
        .expect_err("legacy --openshell-endpoint must be rejected");
        assert!(error.to_string().contains("--openshell-endpoint"));
    }

    #[test]
    fn accepts_gateway_otlp_configuration() {
        let args = Args::try_parse_from([
            "openshell-driver-vm",
            "--otlp-endpoint",
            "http://127.0.0.1:4317",
            "--gateway-name",
            "production-us-west",
        ]);
        assert!(
            args.is_ok(),
            "VM driver should accept the gateway OTLP endpoint"
        );
    }

    #[test]
    fn listen_mode_rejects_bind_address_without_tcp_opt_in() {
        let args = Args::parse_from(["openshell-driver-vm", "--bind-address", "127.0.0.1:50061"]);
        let err =
            compute_driver_listen_mode(&args).expect_err("TCP bind should require explicit opt-in");
        assert!(err.contains("--allow-unauthenticated-tcp"));
    }

    #[test]
    fn listen_mode_requires_bind_address_with_tcp_opt_in() {
        let args = Args::parse_from(["openshell-driver-vm", "--allow-unauthenticated-tcp"]);
        let err =
            compute_driver_listen_mode(&args).expect_err("TCP opt-in should require an address");
        assert!(err.contains("--bind-address is required"));
    }

    #[test]
    fn listen_mode_accepts_explicit_unauthenticated_tcp() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--allow-unauthenticated-tcp",
            "--bind-address",
            "127.0.0.1:50061",
        ]);
        assert_eq!(
            compute_driver_listen_mode(&args).unwrap(),
            ComputeDriverListenMode::Tcp("127.0.0.1:50061".parse().unwrap())
        );
    }

    #[test]
    fn listen_mode_requires_expected_peer_pid_for_uds() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--bind-socket",
            "/tmp/compute-driver.sock",
        ]);
        let err = compute_driver_listen_mode(&args)
            .expect_err("UDS should require gateway peer pid by default");
        assert!(err.contains("--expected-peer-pid is required"));
    }

    #[test]
    fn listen_mode_accepts_uds_with_expected_peer_pid() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--bind-socket",
            "/tmp/compute-driver.sock",
            "--expected-peer-pid",
            "42",
        ]);
        assert_eq!(
            compute_driver_listen_mode(&args).unwrap(),
            ComputeDriverListenMode::Unix {
                socket_path: PathBuf::from("/tmp/compute-driver.sock"),
                expected_peer_pid: Some(42),
            }
        );
    }

    #[test]
    fn listen_mode_accepts_explicit_same_uid_uds_dev_mode() {
        let args = Args::parse_from([
            "openshell-driver-vm",
            "--bind-socket",
            "/tmp/compute-driver.sock",
            "--allow-same-uid-peer",
        ]);
        assert_eq!(
            compute_driver_listen_mode(&args).unwrap(),
            ComputeDriverListenMode::Unix {
                socket_path: PathBuf::from("/tmp/compute-driver.sock"),
                expected_peer_pid: None,
            }
        );
    }
}
