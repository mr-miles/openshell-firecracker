// SPDX-FileCopyrightText: Copyright (c) 2026 openshell-firecracker contributors
// SPDX-License-Identifier: Apache-2.0

//! The driver's JWT-SVID signing key: an ES256 (P-256) key kept in the driver's
//! state directory, and the JWS encoding and verification the Workload API
//! and the JWKS endpoints need.
//!
//! ES256 rather than the gateway's `EdDSA`: the SPIFFE JWT-SVID specification
//! admits only the RS, ES and PS families, and relying parties (OAuth token
//! services) commonly support nothing else.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _,
    UnparsedPublicKey,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest as _, Sha256};

pub(super) const JWT_ALGORITHM: &str = "ES256";
/// Uncompressed SEC1 point: `0x04 || X || Y`.
const P256_POINT_LEN: usize = 65;
const P256_COORDINATE_LEN: usize = 32;

pub(super) struct Signer {
    key: EcdsaKeyPair,
    rng: SystemRandom,
    kid: String,
    x: String,
    y: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// Load the key at `path`, or create it (mode 0600) if there is none.
    ///
    /// An existing key readable by group or others is refused rather than
    /// silently used: whoever can read it can mint identities for every
    /// sandbox.
    pub(super) fn load_or_create(path: &Path) -> Result<Self, String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(format!(
                        "supervisor identity signing key {} is not a regular file",
                        path.display()
                    ));
                }
                if metadata.mode() & 0o077 != 0 {
                    return Err(format!(
                        "supervisor identity signing key {} is accessible to other users (mode {:o}); restrict it to 0600",
                        path.display(),
                        metadata.mode() & 0o777
                    ));
                }
                let bytes = fs::read(path).map_err(|error| {
                    format!(
                        "read supervisor identity signing key {}: {error}",
                        path.display()
                    )
                })?;
                Self::from_pkcs8(&bytes).map_err(|error| {
                    format!(
                        "supervisor identity signing key {}: {error}",
                        path.display()
                    )
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let pkcs8 = generate_pkcs8()?;
                write_private_file(path, &pkcs8)?;
                Self::from_pkcs8(&pkcs8)
            }
            Err(error) => Err(format!(
                "inspect supervisor identity signing key {}: {error}",
                path.display()
            )),
        }
    }

    pub(super) fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8)
            .map_err(|error| format!("not a PKCS#8 P-256 private key: {error}"))?;
        let point = key.public_key().as_ref();
        if point.len() != P256_POINT_LEN || point[0] != 0x04 {
            return Err("unexpected P-256 public key encoding".to_string());
        }
        let x = URL_SAFE_NO_PAD.encode(&point[1..=P256_COORDINATE_LEN]);
        let y = URL_SAFE_NO_PAD.encode(&point[1 + P256_COORDINATE_LEN..]);
        let kid = jwk_thumbprint(&x, &y);
        Ok(Self {
            key,
            rng: SystemRandom::new(),
            kid,
            x,
            y,
        })
    }

    pub(super) fn kid(&self) -> &str {
        &self.kid
    }

    /// The public key as a JWK. `use_` is `sig` for OIDC discovery and
    /// `jwt-svid` in a SPIFFE bundle.
    pub(super) fn public_jwk(&self, use_: &str) -> serde_json::Value {
        serde_json::json!({
            "kty": "EC",
            "crv": "P-256",
            "x": self.x,
            "y": self.y,
            "kid": self.kid,
            "alg": JWT_ALGORITHM,
            "use": use_,
        })
    }

    /// Sign `claims` as a compact JWS.
    pub(super) fn sign(&self, claims: &serde_json::Value) -> Result<String, String> {
        let header = serde_json::json!({
            "alg": JWT_ALGORITHM,
            "kid": self.kid,
            "typ": "JWT",
        });
        let header = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&header).map_err(|error| format!("encode JWT header: {error}"))?,
        );
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(claims).map_err(|error| format!("encode JWT claims: {error}"))?,
        );
        let signing_input = format!("{header}.{payload}");
        let signature = self
            .key
            .sign(&self.rng, signing_input.as_bytes())
            .map_err(|_| "sign JWT-SVID".to_string())?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }

    /// Verify a compact JWS signed by this key and return its claims. Claim
    /// semantics (expiry, audience) are left to the caller.
    pub(super) fn verify(
        &self,
        token: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, String> {
        let mut parts = token.split('.');
        let (Some(encoded_header), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("malformed JWT".to_string());
        };
        let header: serde_json::Value = decode_segment(encoded_header, "header")?;
        if header.get("alg").and_then(serde_json::Value::as_str) != Some(JWT_ALGORITHM) {
            return Err("unsupported JWT algorithm".to_string());
        }
        if header.get("kid").and_then(serde_json::Value::as_str) != Some(self.kid.as_str()) {
            return Err("JWT signed by an unknown key".to_string());
        }
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| "malformed JWT signature".to_string())?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, self.key.public_key().as_ref())
            .verify(format!("{encoded_header}.{payload}").as_bytes(), &signature)
            .map_err(|_| "JWT signature verification failed".to_string())?;
        match decode_segment(payload, "claims")? {
            serde_json::Value::Object(claims) => Ok(claims),
            _ => Err("JWT claims are not an object".to_string()),
        }
    }
}

fn decode_segment(segment: &str, what: &str) -> Result<serde_json::Value, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| format!("malformed JWT {what}"))?;
    serde_json::from_slice(&bytes).map_err(|_| format!("malformed JWT {what}"))
}

/// RFC 7638 thumbprint of a P-256 JWK: the required members in lexicographic
/// order, without whitespace.
fn jwk_thumbprint(x: &str, y: &str) -> String {
    let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

fn generate_pkcs8() -> Result<Vec<u8>, String> {
    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
        .map(|document| document.as_ref().to_vec())
        .map_err(|_| "generate supervisor identity signing key".to_string())
}

/// Write `bytes` to `path` as a new 0600 file, atomically.
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    let _ = fs::remove_file(&temporary);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        // The mode passed to open() is filtered by the umask; 0600 is the
        // most it can be, but make it exact.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies_round_trip() {
        let signer = Signer::from_pkcs8(&generate_pkcs8().unwrap()).unwrap();
        let token = signer
            .sign(&serde_json::json!({"sub": "spiffe://example.org/a"}))
            .unwrap();
        let claims = signer.verify(&token).unwrap();
        assert_eq!(claims["sub"], "spiffe://example.org/a");
    }

    #[test]
    fn rejects_tampered_payload_and_foreign_key() {
        let signer = Signer::from_pkcs8(&generate_pkcs8().unwrap()).unwrap();
        let other = Signer::from_pkcs8(&generate_pkcs8().unwrap()).unwrap();
        let token = signer
            .sign(&serde_json::json!({"sub": "spiffe://example.org/a"}))
            .unwrap();
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(br#"{"sub":"spiffe://example.org/b"}"#);
        parts[1] = &forged;
        assert!(signer.verify(&parts.join(".")).is_err());
        assert!(other.verify(&token).is_err());
        assert!(signer.verify("a.b").is_err());
    }

    #[test]
    fn key_persists_with_private_mode_and_stable_kid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.p8");
        let first = Signer::load_or_create(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let second = Signer::load_or_create(&path).unwrap();
        assert_eq!(first.kid(), second.kid());
    }

    #[test]
    fn refuses_key_readable_by_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.p8");
        Signer::load_or_create(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = Signer::load_or_create(&path).unwrap_err();
        assert!(error.contains("accessible to other users"), "{error}");
    }

    #[test]
    fn jwk_thumbprint_matches_rfc7638_shape() {
        let signer = Signer::from_pkcs8(&generate_pkcs8().unwrap()).unwrap();
        let jwk = signer.public_jwk("sig");
        let expected = jwk_thumbprint(jwk["x"].as_str().unwrap(), jwk["y"].as_str().unwrap());
        assert_eq!(jwk["kid"], expected);
        // SHA-256 is 32 bytes: 43 base64url characters unpadded.
        assert_eq!(signer.kid().len(), 43);
    }
}
