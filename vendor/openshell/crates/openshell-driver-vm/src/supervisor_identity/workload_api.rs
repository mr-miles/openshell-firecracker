// SPDX-FileCopyrightText: Copyright (c) 2026 openshell-firecracker contributors
// SPDX-License-Identifier: Apache-2.0

//! The JWT half of the SPIFFE Workload API (`SpiffeWorkloadAPI` in SPIFFE's
//! `workload.proto`), served on one sandbox's socket.
//!
//! `FetchJWTSVID`, `FetchJWTBundles` and `ValidateJWTSVID` are implemented;
//! the X.509 calls answer `Unimplemented`. Every call must carry the
//! `workload.spiffe.io: true` metadata the specification requires and come
//! from the sandbox's attested supervisor ([`super::attest`]).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt as _};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::server::UdsConnectInfo;
use tonic::{Request, Response, Status};
use tracing::{debug, warn};

use super::SupervisorIdentity;
use super::attest::ExpectedSupervisor;

/// Hand-written messages for the parts of `workload.proto` the driver serves,
/// with the field numbers of the specification. The service stub is generated
/// by `build.rs` without `protoc`.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    unused_qualifications,
    trivial_casts
)]
pub(super) mod proto {
    use std::collections::HashMap;

    /// X.509 SVIDs are not issued; the X.509 messages carry no fields
    /// because the calls that would send them are refused.
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct X509SvidRequest {}
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct X509SvidResponse {}
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct X509BundlesRequest {}
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct X509BundlesResponse {}

    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct JwtSvidRequest {
        #[prost(string, repeated, tag = "1")]
        pub audience: Vec<String>,
        #[prost(string, tag = "2")]
        pub spiffe_id: String,
    }
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct JwtSvidResponse {
        #[prost(message, repeated, tag = "1")]
        pub svids: Vec<JwtSvid>,
    }
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct JwtSvid {
        #[prost(string, tag = "1")]
        pub spiffe_id: String,
        #[prost(string, tag = "2")]
        pub svid: String,
        #[prost(string, tag = "3")]
        pub hint: String,
    }
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct JwtBundlesRequest {}
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct JwtBundlesResponse {
        #[prost(map = "string, bytes", tag = "1")]
        pub bundles: HashMap<String, Vec<u8>>,
    }
    #[derive(Clone, PartialEq, Eq, ::prost::Message)]
    pub struct ValidateJwtSvidRequest {
        #[prost(string, tag = "1")]
        pub audience: String,
        #[prost(string, tag = "2")]
        pub svid: String,
    }
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct ValidateJwtSvidResponse {
        #[prost(string, tag = "1")]
        pub spiffe_id: String,
        #[prost(message, optional, tag = "2")]
        pub claims: Option<::prost_types::Struct>,
    }

    include!(concat!(env!("OUT_DIR"), "/.SpiffeWorkloadAPI.rs"));
}

// The route names are SPIFFE's (`/SpiffeWorkloadAPI/...`), and the generated
// Rust names follow them.
use proto::spiffe_workload_a_p_i_server::{
    SpiffeWorkloadAPI as SpiffeWorkloadApi, SpiffeWorkloadAPIServer as SpiffeWorkloadApiServer,
};

const SECURITY_HEADER: &str = "workload.spiffe.io";
/// Bounds on a JWT-SVID request, so a request cannot make the driver sign an
/// arbitrarily large token.
const MAX_AUDIENCES: usize = 16;
const MAX_AUDIENCE_LEN: usize = 2048;

type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

pub(super) struct WorkloadApi {
    identity: Arc<SupervisorIdentity>,
    sandbox_id: String,
    expected: ExpectedSupervisor,
}

impl WorkloadApi {
    pub(super) fn new(
        identity: Arc<SupervisorIdentity>,
        sandbox_id: String,
        expected: ExpectedSupervisor,
    ) -> Self {
        Self {
            identity,
            sandbox_id,
            expected,
        }
    }

    /// Serve this sandbox's socket until the task is aborted.
    pub(super) fn spawn(self, listener: tokio::net::UnixListener) -> JoinHandle<()> {
        let sandbox_id = self.sandbox_id.clone();
        tokio::spawn(async move {
            let result = tonic::transport::Server::builder()
                .add_service(SpiffeWorkloadApiServer::new(self))
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await;
            if let Err(error) = result {
                warn!(sandbox_id = %sandbox_id, %error, "workload API server stopped");
            }
        })
    }

    #[allow(clippy::result_large_err)]
    fn authorize<T>(&self, request: &Request<T>, call: &str) -> Result<(), Status> {
        if request
            .metadata()
            .get(SECURITY_HEADER)
            .and_then(|value| value.to_str().ok())
            != Some("true")
        {
            return Err(Status::invalid_argument(
                "security header missing from request",
            ));
        }
        let credentials = request
            .extensions()
            .get::<UdsConnectInfo>()
            .and_then(|info| info.peer_cred);
        let Some(credentials) = credentials else {
            warn!(sandbox_id = %self.sandbox_id, call, "workload API peer credentials unavailable");
            return Err(Status::permission_denied("workload attestation failed"));
        };
        self.expected
            .verify(credentials.pid(), credentials.uid())
            .map_err(|reason| {
                warn!(
                    sandbox_id = %self.sandbox_id,
                    call,
                    peer_pid = ?credentials.pid(),
                    peer_uid = credentials.uid(),
                    %reason,
                    "workload API request refused"
                );
                Status::permission_denied("workload attestation failed")
            })
    }
}

#[tonic::async_trait]
impl SpiffeWorkloadApi for WorkloadApi {
    type FetchX509SVIDStream = ResponseStream<proto::X509SvidResponse>;
    type FetchX509BundlesStream = ResponseStream<proto::X509BundlesResponse>;
    type FetchJWTBundlesStream = ResponseStream<proto::JwtBundlesResponse>;

    async fn fetch_x509_svid(
        &self,
        _request: Request<proto::X509SvidRequest>,
    ) -> Result<Response<Self::FetchX509SVIDStream>, Status> {
        Err(Status::unimplemented("X.509 SVIDs are not issued"))
    }

    async fn fetch_x509_bundles(
        &self,
        _request: Request<proto::X509BundlesRequest>,
    ) -> Result<Response<Self::FetchX509BundlesStream>, Status> {
        Err(Status::unimplemented("X.509 SVIDs are not issued"))
    }

    async fn fetch_jwt_svid(
        &self,
        request: Request<proto::JwtSvidRequest>,
    ) -> Result<Response<proto::JwtSvidResponse>, Status> {
        self.authorize(&request, "FetchJWTSVID")?;
        let request = request.into_inner();
        if request.audience.is_empty() {
            return Err(Status::invalid_argument("audience must be specified"));
        }
        if request.audience.len() > MAX_AUDIENCES
            || request
                .audience
                .iter()
                .any(|audience| audience.is_empty() || audience.len() > MAX_AUDIENCE_LEN)
        {
            return Err(Status::invalid_argument(format!(
                "audience must be 1 to {MAX_AUDIENCES} non-empty values of at most {MAX_AUDIENCE_LEN} bytes"
            )));
        }
        let spiffe_id = self.identity.spiffe_id(&self.sandbox_id);
        if !request.spiffe_id.is_empty() && request.spiffe_id != spiffe_id {
            return Err(Status::permission_denied(
                "no identity issued for the requested SPIFFE ID",
            ));
        }
        let svid = self
            .identity
            .mint(&self.sandbox_id, &request.audience)
            .map_err(|error| {
                warn!(sandbox_id = %self.sandbox_id, %error, "mint JWT-SVID failed");
                Status::internal("failed to issue JWT-SVID")
            })?;
        debug!(
            sandbox_id = %self.sandbox_id,
            audience = ?request.audience,
            "issued JWT-SVID"
        );
        Ok(Response::new(proto::JwtSvidResponse {
            svids: vec![proto::JwtSvid {
                spiffe_id,
                svid,
                hint: String::new(),
            }],
        }))
    }

    async fn fetch_jwt_bundles(
        &self,
        request: Request<proto::JwtBundlesRequest>,
    ) -> Result<Response<Self::FetchJWTBundlesStream>, Status> {
        self.authorize(&request, "FetchJWTBundles")?;
        let bundle = self.identity.spiffe_bundle().map_err(|error| {
            warn!(%error, "encode JWT bundle failed");
            Status::internal("failed to encode JWT bundle")
        })?;
        let response = proto::JwtBundlesResponse {
            bundles: HashMap::from([(self.identity.trust_domain().to_string(), bundle)]),
        };
        // A Workload API bundle stream stays open for updates. The key does
        // not rotate while the driver runs, so nothing follows the first.
        let stream =
            futures::stream::once(async move { Ok(response) }).chain(futures::stream::pending());
        Ok(Response::new(Box::pin(stream)))
    }

    async fn validate_jwt_svid(
        &self,
        request: Request<proto::ValidateJwtSvidRequest>,
    ) -> Result<Response<proto::ValidateJwtSvidResponse>, Status> {
        self.authorize(&request, "ValidateJWTSVID")?;
        let request = request.into_inner();
        if request.audience.is_empty() {
            return Err(Status::invalid_argument("audience must be specified"));
        }
        if request.svid.is_empty() {
            return Err(Status::invalid_argument("svid must be specified"));
        }
        let (spiffe_id, claims) = self
            .identity
            .validate(&request.svid, &request.audience)
            .map_err(Status::invalid_argument)?;
        Ok(Response::new(proto::ValidateJwtSvidResponse {
            spiffe_id,
            claims: Some(json_object_to_struct(claims)),
        }))
    }
}

fn json_object_to_struct(
    object: serde_json::Map<String, serde_json::Value>,
) -> prost_types::Struct {
    prost_types::Struct {
        fields: object
            .into_iter()
            .map(|(key, value)| (key, json_to_value(value)))
            .collect(),
    }
}

fn json_to_value(value: serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(value) => Kind::BoolValue(value),
        serde_json::Value::Number(value) => Kind::NumberValue(value.as_f64().unwrap_or_default()),
        serde_json::Value::String(value) => Kind::StringValue(value),
        serde_json::Value::Array(values) => Kind::ListValue(prost_types::ListValue {
            values: values.into_iter().map(json_to_value).collect(),
        }),
        serde_json::Value::Object(object) => Kind::StructValue(json_object_to_struct(object)),
    };
    prost_types::Value { kind: Some(kind) }
}
