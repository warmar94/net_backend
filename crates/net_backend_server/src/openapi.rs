//! The OpenAPI document (utoipa): the core routes, every module's documented routes and the
//! game's, served at `/v1/openapi.json`.
//!
//! Handlers become part of the document when they carry `#[utoipa::path(..)]` and are registered
//! with `utoipa_axum::routes!` ([`NetBackendServer::routes`](crate::NetBackendServer::routes),
//! or a module's [`routes`](crate::Module::routes)); plain `.route(..)` handlers work but stay
//! undocumented. The protocol crate has no OpenAPI dependency, so the schemas of its types are
//! described here by mirror types whose shape is checked against the real types in tests.

use serde::Serialize;
use utoipa::openapi::{InfoBuilder, OpenApi, OpenApiBuilder};
use utoipa::ToSchema;

use crate::config::Config;

/// Mirror of the protocol's `ServerInfo` (checked against it in tests).
#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub(crate) struct ServerInfo {
    /// The newest protocol version the server speaks.
    protocol: u32,
    /// The oldest protocol version the server still accepts.
    min_protocol: u32,
    /// The enabled modules.
    modules: Vec<String>,
}

/// Mirror of the protocol's `ApiError`.
#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub(crate) struct ApiError {
    /// The stable error code (`not_found`, `validation_failed`, …).
    code: String,
    /// A human-readable message; may change, clients branch on `code`.
    message: String,
    /// Code-specific details.
    #[schema(value_type = Option<Object>)]
    details: Option<serde_json::Value>,
}

/// Mirror of the protocol's `ErrorBody`: the body of every 4xx / 5xx answer.
#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub(crate) struct ErrorBody {
    /// The error.
    error: ApiError,
}

/// `{"status":"ok"}` / `{"status":"ready"}`.
#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
pub(crate) struct Status {
    /// `ok` or `ready`.
    status: String,
}

/// The base document: title and version from the config, the shared error schemas.
pub(crate) fn base(config: &Config) -> OpenApi {
    let info = InfoBuilder::new().title(config.openapi.title.clone()).version(config.openapi.version.clone()).build();
    let mut doc = OpenApiBuilder::new().info(info).build();
    let components = doc.components.get_or_insert_with(Default::default);
    components.schemas.insert("ErrorBody".into(), <ErrorBody as utoipa::PartialSchema>::schema());
    components.schemas.insert("ApiError".into(), <ApiError as utoipa::PartialSchema>::schema());
    doc
}
