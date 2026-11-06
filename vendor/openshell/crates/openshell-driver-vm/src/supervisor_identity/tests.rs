// SPDX-FileCopyrightText: Copyright (c) 2026 openshell-firecracker contributors
// SPDX-License-Identifier: Apache-2.0

use super::*;

const CHILD_SOCKET_ENV: &str = "OPENSHELL_TEST_WORKLOAD_API_CHILD_SOCKET";
const CHILD_TEST: &str = "supervisor_identity::tests::workload_api_child";
const AUDIENCE: &str = "https://auth.example.test";

fn enabled_config() -> SupervisorIdentityConfig {
    SupervisorIdentityConfig {
        enabled: true,
        trust_domain: "example.test".to_string(),
        ..SupervisorIdentityConfig::default()
    }
}

fn load(dir: &Path, config: &SupervisorIdentityConfig) -> Arc<SupervisorIdentity> {
    SupervisorIdentity::load(config, dir, &dir.join("probe.sock")).unwrap()
}

fn claims_of(token: &str) -> serde_json::Value {
    use base64::Engine as _;
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap()
}

/// A private directory short enough for a socket path, on the same
/// filesystem as the test binary where possible so the supervisor link is a
/// hard link rather than a copy of a large binary.
fn sandbox_dir() -> tempfile::TempDir {
    let beside_binary = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|dir| tempfile::tempdir_in(dir).ok())
        .filter(|dir| dir.path().join(SOCKET_FILE).as_os_str().len() <= SOCKET_PATH_MAX);
    beside_binary.unwrap_or_else(|| tempfile::tempdir().unwrap())
}

#[test]
fn default_config_is_disabled_and_validates_to_local_issuer() {
    let config = SupervisorIdentityConfig::default();
    assert!(!config.enabled);
    assert_eq!(config.validate().unwrap(), "http://127.0.0.1:8961");
}

#[test]
fn config_validation_rejects_bad_values() {
    let bad_domain = SupervisorIdentityConfig {
        trust_domain: "Example.org".to_string(),
        ..enabled_config()
    };
    assert!(bad_domain.validate().is_err());

    for ttl in [MIN_SVID_TTL_SECS - 1, MAX_SVID_TTL_SECS + 1] {
        let config = SupervisorIdentityConfig {
            svid_ttl_secs: ttl,
            ..enabled_config()
        };
        assert!(config.validate().is_err(), "ttl {ttl}");
    }

    let wildcard = SupervisorIdentityConfig {
        jwks_bind: "0.0.0.0:8961".parse().unwrap(),
        ..enabled_config()
    };
    let error = wildcard.validate().unwrap_err();
    assert!(error.contains("set the issuer"), "{error}");

    for issuer in [
        "ftp://issuer.example",
        "https://issuer.example/?a=b",
        "https://user@issuer.example",
        "not a url",
    ] {
        let config = SupervisorIdentityConfig {
            issuer: Some(issuer.to_string()),
            ..enabled_config()
        };
        assert!(config.validate().is_err(), "{issuer}");
    }
}

#[test]
fn issuer_trailing_slash_is_dropped() {
    let config = SupervisorIdentityConfig {
        issuer: Some("https://issuer.example/openshell/".to_string()),
        jwks_bind: "0.0.0.0:8961".parse().unwrap(),
        ..enabled_config()
    };
    assert_eq!(
        config.validate().unwrap(),
        "https://issuer.example/openshell"
    );
}

#[test]
fn load_refuses_a_state_dir_too_deep_for_sockets() {
    let dir = tempfile::tempdir().unwrap();
    let deep = dir.path().join("x".repeat(120)).join(SOCKET_FILE);
    let error = SupervisorIdentity::load(&enabled_config(), dir.path(), &deep).unwrap_err();
    assert!(error.contains("shorter driver state directory"), "{error}");
}

#[test]
fn minted_svid_carries_sandbox_identity_and_validates() {
    let dir = tempfile::tempdir().unwrap();
    let identity = load(dir.path(), &enabled_config());

    let token = identity.mint("sb-1", &[AUDIENCE.to_string()]).unwrap();
    let claims = claims_of(&token);
    assert_eq!(claims["iss"], "http://127.0.0.1:8961");
    assert_eq!(
        claims["sub"],
        "spiffe://example.test/openshell/sandbox/sb-1"
    );
    assert_eq!(claims["aud"], AUDIENCE);
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        DEFAULT_SVID_TTL_SECS
    );

    let (spiffe_id, _) = identity.validate(&token, AUDIENCE).unwrap();
    assert_eq!(spiffe_id, "spiffe://example.test/openshell/sandbox/sb-1");
    assert!(identity.validate(&token, "https://other.test").is_err());

    let both = identity
        .mint("sb-1", &[AUDIENCE.to_string(), "b".to_string()])
        .unwrap();
    assert_eq!(claims_of(&both)["aud"], serde_json::json!([AUDIENCE, "b"]));
    identity.validate(&both, "b").unwrap();
}

#[test]
fn validate_rejects_another_issuers_token_from_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let first = load(dir.path(), &enabled_config());
    let second = load(
        dir.path(),
        &SupervisorIdentityConfig {
            issuer: Some("https://elsewhere.example".to_string()),
            ..enabled_config()
        },
    );
    let token = second.mint("sb-1", &[AUDIENCE.to_string()]).unwrap();
    assert!(first.validate(&token, AUDIENCE).is_err());
}

#[test]
fn discovery_document_points_at_the_jwks() {
    let dir = tempfile::tempdir().unwrap();
    let identity = load(dir.path(), &enabled_config());
    let document = identity.discovery_document();
    assert_eq!(document["issuer"], "http://127.0.0.1:8961");
    assert_eq!(
        document["jwks_uri"],
        "http://127.0.0.1:8961/.well-known/jwks.json"
    );
    assert_eq!(
        document["id_token_signing_alg_values_supported"],
        serde_json::json!(["ES256"])
    );
}

#[test]
fn prepare_links_binary_binds_socket_and_cleans_up() {
    let driver = tempfile::tempdir().unwrap();
    let identity = load(driver.path(), &enabled_config());
    let sandbox = tempfile::tempdir().unwrap();
    let binary = sandbox.path().join("shared-supervisor");
    fs::write(&binary, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    // Leftovers from an earlier launch are replaced.
    fs::write(sandbox.path().join(SOCKET_FILE), b"stale").unwrap();
    fs::write(sandbox.path().join(SUPERVISOR_LINK_FILE), b"stale").unwrap();

    let prepared = identity.prepare_sandbox(sandbox.path(), &binary).unwrap();
    let link = fs::metadata(prepared.supervisor_path()).unwrap();
    let source = fs::metadata(&binary).unwrap();
    assert_eq!((link.dev(), link.ino()), (source.dev(), source.ino()));
    let socket = fs::symlink_metadata(prepared.socket_path()).unwrap();
    assert!(std::os::unix::fs::FileTypeExt::is_socket(
        &socket.file_type()
    ));
    assert_eq!(socket.mode() & 0o777, 0o600);

    drop(prepared);
    assert!(!sandbox.path().join(SOCKET_FILE).exists());
    assert!(!sandbox.path().join(SUPERVISOR_LINK_FILE).exists());
    assert!(binary.exists());
}

#[test]
fn remove_sandbox_artifacts_leaves_other_files() {
    let sandbox = tempfile::tempdir().unwrap();
    fs::write(sandbox.path().join(SOCKET_FILE), b"").unwrap();
    fs::write(sandbox.path().join(SUPERVISOR_LINK_FILE), b"").unwrap();
    fs::write(sandbox.path().join("supervisor.log"), b"").unwrap();
    remove_sandbox_artifacts(sandbox.path());
    assert!(!sandbox.path().join(SOCKET_FILE).exists());
    assert!(!sandbox.path().join(SUPERVISOR_LINK_FILE).exists());
    assert!(sandbox.path().join("supervisor.log").exists());
}

/// Run by [`attested_supervisor_fetches_svid_over_the_socket`] as the
/// stand-in supervisor: the test binary, started from the sandbox's link.
/// It talks to the socket with the `spiffe` client the real supervisor uses.
#[tokio::test]
#[ignore = "child process of attested_supervisor_fetches_svid_over_the_socket"]
async fn workload_api_child() {
    let Some(socket) = std::env::var_os(CHILD_SOCKET_ENV) else {
        return;
    };
    let endpoint = openshell_core::spiffe::workload_api_endpoint(Path::new(&socket));
    let client = spiffe::WorkloadApiClient::connect_to(&endpoint)
        .await
        .expect("connect");
    let token = client
        .fetch_jwt_token([AUDIENCE], None)
        .await
        .expect("fetch JWT-SVID");
    let bundles = client.fetch_jwt_bundles().await.expect("fetch JWT bundles");
    let svid = spiffe::JwtSvid::parse_and_validate(&token, &bundles, &[AUDIENCE])
        .expect("JWT-SVID verifies against the Workload API bundle");
    let validated = client
        .validate_jwt_token(AUDIENCE, &token)
        .await
        .expect("ValidateJWTSVID");
    assert_eq!(validated.spiffe_id(), svid.spiffe_id());
    println!("CHILD-SVID {token}");
}

#[tokio::test]
async fn attested_supervisor_fetches_svid_over_the_socket() {
    let driver = tempfile::tempdir().unwrap();
    let identity = load(driver.path(), &enabled_config());
    let sandbox = sandbox_dir();
    let prepared = identity
        .prepare_sandbox(sandbox.path(), &std::env::current_exe().unwrap())
        .unwrap();
    let socket = prepared.socket_path().to_path_buf();

    let child = tokio::process::Command::new(prepared.supervisor_path())
        .args([
            CHILD_TEST,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SOCKET_ENV, &socket)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let endpoint = prepared
        .start(identity.clone(), "sb-e2e", child.id().unwrap())
        .unwrap();

    let output = tokio::time::timeout(std::time::Duration::from_mins(1), child.wait_with_output())
        .await
        .expect("child finished")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "child failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // libtest may print the test name on the same line.
    let token = stdout
        .split_once("CHILD-SVID ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no token in child output: {stdout}"));
    let (spiffe_id, _) = identity.validate(token, AUDIENCE).unwrap();
    assert_eq!(spiffe_id, "spiffe://example.test/openshell/sandbox/sb-e2e");

    drop(endpoint);
    assert!(!socket.exists());
    assert!(!sandbox.path().join(SUPERVISOR_LINK_FILE).exists());
}

#[tokio::test]
async fn other_processes_are_refused() {
    let driver = tempfile::tempdir().unwrap();
    let identity = load(driver.path(), &enabled_config());
    let sandbox = sandbox_dir();
    let binary = sandbox.path().join("shared-supervisor");
    fs::copy("/bin/sleep", &binary).unwrap();
    let prepared = identity.prepare_sandbox(sandbox.path(), &binary).unwrap();
    // The real supervisor: a process started from the link.
    let mut supervisor = tokio::process::Command::new(prepared.supervisor_path())
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let socket = prepared.socket_path().to_path_buf();
    let _endpoint = prepared
        .start(identity, "sb-1", supervisor.id().unwrap())
        .unwrap();

    // This test process is not it.
    let client = spiffe::WorkloadApiClient::connect_to(
        openshell_core::spiffe::workload_api_endpoint(&socket),
    )
    .await
    .unwrap();
    let error = client.fetch_jwt_token([AUDIENCE], None).await.unwrap_err();
    assert!(
        matches!(error, spiffe::WorkloadApiError::PermissionDenied(_))
            || error.to_string().contains("attestation"),
        "{error:?}"
    );
    supervisor.kill().await.unwrap();
}

#[tokio::test]
async fn start_refuses_a_process_not_started_from_the_link() {
    let driver = tempfile::tempdir().unwrap();
    let identity = load(driver.path(), &enabled_config());
    let sandbox = sandbox_dir();
    let binary = sandbox.path().join("shared-supervisor");
    fs::copy("/bin/sleep", &binary).unwrap();
    let prepared = identity.prepare_sandbox(sandbox.path(), &binary).unwrap();
    // This test process does not run the link.
    let error = prepared
        .start(identity, "sb-1", std::process::id())
        .unwrap_err();
    assert!(error.contains("is not running"), "{error}");
    assert!(!sandbox.path().join(SOCKET_FILE).exists());
    assert!(!sandbox.path().join(SUPERVISOR_LINK_FILE).exists());
}

async fn http_get(address: SocketAddr, path: &str) -> (String, serde_json::Value) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (head.to_string(), serde_json::from_str(body).unwrap())
}

#[tokio::test]
async fn jwks_endpoints_publish_a_key_that_verifies_minted_svids() {
    let dir = tempfile::tempdir().unwrap();
    let identity = load(dir.path(), &enabled_config());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(identity.clone().serve_jwks(listener, async {
        let _ = stopped.await;
    }));

    let (head, discovery) = http_get(address, jwks::DISCOVERY_PATH).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert_eq!(discovery["issuer"], identity.issuer());

    let (head, keys) = http_get(address, jwks::JWKS_PATH).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(keys["keys"][0].get("d").is_none(), "private key published");

    // A standard verifier, given only the published JWKS, accepts the token.
    let trust_domain = spiffe::TrustDomain::new("example.test").unwrap();
    let bundle =
        spiffe::JwtBundle::from_jwt_authorities(trust_domain, keys.to_string().as_bytes()).unwrap();
    let mut bundles = spiffe::JwtBundleSet::new();
    bundles.add_bundle(bundle);
    let token = identity.mint("sb-1", &[AUDIENCE.to_string()]).unwrap();
    let svid = spiffe::JwtSvid::parse_and_validate(&token, &bundles, &[AUDIENCE]).unwrap();
    assert_eq!(
        svid.spiffe_id().to_string(),
        "spiffe://example.test/openshell/sandbox/sb-1"
    );

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
