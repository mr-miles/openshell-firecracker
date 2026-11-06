// SPDX-FileCopyrightText: Copyright (c) 2026 openshell-firecracker contributors
// SPDX-License-Identifier: Apache-2.0

//! HTTP endpoints from which relying parties verify the driver's JWT-SVIDs:
//! an `OpenID` Connect discovery document and the JWKS it points to, in the
//! shape SPIRE's OIDC discovery provider publishes.
//!
//! Served at the root of the listener. A reverse proxy that publishes them
//! under an issuer URL with a path must strip that path.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::SupervisorIdentity;

pub(super) const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
pub(super) const JWKS_PATH: &str = "/.well-known/jwks.json";
/// The signing key does not change while the driver runs, so relying parties
/// may cache it briefly.
const CACHE_CONTROL: &str = "public, max-age=300";

pub(super) fn router(identity: Arc<SupervisorIdentity>) -> Router {
    Router::new()
        .route(DISCOVERY_PATH, get(discovery))
        .route(JWKS_PATH, get(jwks))
        .with_state(identity)
}

async fn discovery(State(identity): State<Arc<SupervisorIdentity>>) -> Response {
    json_response(&identity.discovery_document())
}

async fn jwks(State(identity): State<Arc<SupervisorIdentity>>) -> Response {
    json_response(&identity.oidc_jwks())
}

fn json_response(body: &serde_json::Value) -> Response {
    serde_json::to_vec(body).map_or_else(
        |_| StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        |bytes| {
            (
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    ),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static(CACHE_CONTROL),
                    ),
                ],
                bytes,
            )
                .into_response()
        },
    )
}
