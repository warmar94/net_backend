//! Typed protocol routes: a handler for an [`HttpCall`] is mounted at the call's own path with the
//! call's own method, takes the decoded call ([`Call<C>`]) and answers the call's own response type
//! ([`Reply<C>`]). Path, method, payload and answer come from the protocol, so they cannot drift
//! between the server, its OpenAPI document and the clients.
//!
//! **Authentication is enforced by the mount:** a route whose `C::ROUTE.auth` is true answers 401
//! (`unauthorized` / `token_expired`, or the authenticator's 403 `banned`) to a request without a
//! valid token BEFORE the handler runs, whether or not the handler takes an
//! [`AuthContext`]. The flag in the protocol (and in the OpenAPI document) is a
//! guarantee, also for a game's own `HttpCall` mounted with
//! [`NetBackendServer::call`](crate::NetBackendServer::call) or [`call_route!`](crate::call_route).
//!
//! ```
//! use net_backend_server::http::call::{Call, CallResult, Reply};
//! use net_backend_server::protocol::chat::{ListRooms, RoomInfo, RoomKind};
//! use net_backend_server::protocol::{Page, RoomId};
//! use net_backend_server::{AuthContext, Config, NetBackendServer};
//!
//! async fn rooms(_who: AuthContext, Call(call): Call<ListRooms>) -> CallResult<ListRooms> {
//!     let _ = call.page;
//!     Ok(Reply::new(Page::new(vec![RoomInfo::new(RoomId(1), RoomKind::Room).with_key("world")], None)))
//! }
//!
//! // Mounted at GET /v1/chat/rooms (from `ListRooms::ROUTE`), undocumented; documented handlers
//! // carry `#[utoipa::path(..)]` and go through `call_route!(ListRooms, rooms)`.
//! let server = NetBackendServer::new(Config::default()).call::<ListRooms, _, _, _>(rooms);
//! # let _ = server;
//! ```

use std::future::Future;

use axum::extract::{FromRequest, FromRequestParts, Query, RawPathParams, Request};
use axum::handler::Handler;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter};
use http::header::{HeaderName, HeaderValue};
use http::HeaderMap;
use net_backend_protocol::http_call::placeholders;
use net_backend_protocol::routes::HttpMethod;
use net_backend_protocol::{HttpCall, PathParams, PayloadKind};
use serde_json::Value;
use utoipa::openapi::path::{HttpMethod as DocMethod, Paths};
use utoipa_axum::router::{OpenApiRouter, UtoipaMethodRouter};

use super::ApiJson;
use crate::auth::{AuthContext, AuthFailure};
use crate::error::AppError;
use crate::state::AppState;

/// The decoded call of a typed route: its path parameters and payload (JSON body or query),
/// shape-checked by the protocol's [`HttpCall::from_parts`]. A malformed body answers 400
/// `bad_request` (or 413 / 415 like [`ApiJson`]), a bad path parameter 400 / 422.
#[derive(Debug)]
pub struct Call<C>(pub C);

impl<S, C> FromRequest<S> for Call<C>
where
    S: Send + Sync,
    C: HttpCall + Send,
    C::Payload: Send,
{
    type Rejection = AppError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (mut parts, body) = request.into_parts();
        let mut params = PathParams::new();
        if !placeholders(C::ROUTE.path).is_empty() {
            let raw =
                RawPathParams::from_request_parts(&mut parts, state).await.map_err(|_| AppError::bad_request("the path parameters are not valid UTF-8"))?;
            for (name, value) in &raw {
                params.insert(name, value);
            }
        }
        let payload: C::Payload = match C::PAYLOAD {
            PayloadKind::Json => ApiJson::<C::Payload>::from_request(Request::from_parts(parts, body), state).await?.0,
            PayloadKind::Query => {
                Query::<C::Payload>::from_request_parts(&mut parts, state).await.map_err(|rejection| AppError::bad_request(rejection.body_text()))?.0
            }
            // Nothing is sent (`NoPayload` decodes from anything).
            _ => serde_json::from_value(Value::Null).map_err(AppError::internal)?,
        };
        C::from_parts(&params, payload).map(Call).map_err(AppError::from)
    }
}

/// The answer of a typed route: `200` with the call's response as JSON, plus optional headers
/// (e.g. an `ETag`).
#[derive(Debug)]
pub struct Reply<C: HttpCall> {
    data: C::Response,
    headers: HeaderMap,
}

impl<C: HttpCall> Reply<C> {
    /// The answer `data`.
    pub fn new(data: C::Response) -> Self {
        Self { data, headers: HeaderMap::new() }
    }

    /// The same answer with a header.
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// The answer's data.
    pub fn data(&self) -> &C::Response {
        &self.data
    }
}

impl<C: HttpCall> IntoResponse for Reply<C> {
    fn into_response(self) -> Response {
        (self.headers, ApiJson(self.data)).into_response()
    }
}

/// What a typed handler returns.
pub type CallResult<C> = Result<Reply<C>, AppError>;

/// A handler for the call `C`: an async function whose LAST argument is [`Call<C>`] (any other
/// extractors before it) and which returns [`CallResult<C>`]. Implemented for such functions with
/// up to eight extractors; `T` is the tuple of the other extractors.
pub trait CallHandler<C, T>: Clone + Send + Sync + Sized + 'static {}

macro_rules! call_handler {
    ($($ty:ident),*) => {
        impl<F, Fut, C, $($ty,)*> CallHandler<C, ($($ty,)*)> for F
        where
            F: Fn($($ty,)* Call<C>) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = CallResult<C>> + Send,
            C: HttpCall,
        {
        }
    };
}

call_handler!();
call_handler!(T1);
call_handler!(T1, T2);
call_handler!(T1, T2, T3);
call_handler!(T1, T2, T3, T4);
call_handler!(T1, T2, T3, T4, T5);
call_handler!(T1, T2, T3, T4, T5, T6);
call_handler!(T1, T2, T3, T4, T5, T6, T7);
call_handler!(T1, T2, T3, T4, T5, T6, T7, T8);

/// The method filter of a protocol method; `None` for a method a newer protocol adds that this
/// server version does not know (never mounted as another method).
fn method_filter(method: HttpMethod) -> Option<MethodFilter> {
    match method {
        HttpMethod::Get => Some(MethodFilter::GET),
        HttpMethod::Post => Some(MethodFilter::POST),
        HttpMethod::Put => Some(MethodFilter::PUT),
        HttpMethod::Patch => Some(MethodFilter::PATCH),
        HttpMethod::Delete => Some(MethodFilter::DELETE),
        _ => None,
    }
}

/// Whether this server version can mount routes of `method`.
pub(crate) fn supported_method(method: HttpMethod) -> bool {
    method_filter(method).is_some()
}

/// The route layer of a `ROUTE.auth` route: no authenticated caller, no handler.
async fn require_auth(request: Request, next: Next) -> Response {
    if request.extensions().get::<AuthContext>().is_some() {
        return next.run(request).await;
    }
    request.extensions().get::<AuthFailure>().map_or_else(AppError::unauthorized, AuthFailure::to_error).into_response()
}

fn doc_method(method: HttpMethod) -> DocMethod {
    match method {
        HttpMethod::Post => DocMethod::Post,
        HttpMethod::Put => DocMethod::Put,
        HttpMethod::Patch => DocMethod::Patch,
        HttpMethod::Delete => DocMethod::Delete,
        _ => DocMethod::Get,
    }
}

/// The method router of `handler` for `C`'s method; with `C::ROUTE.auth`, requests without an
/// authenticated caller are answered 401 before the handler runs. A method this server version
/// does not know mounts nothing (every request: 405; logged as an error;
/// [`NetBackendServer::call`](crate::NetBackendServer::call) refuses it at build).
pub fn method_router<C, H, T, M>(handler: H) -> MethodRouter<AppState>
where
    C: HttpCall,
    H: CallHandler<C, T> + Handler<M, AppState>,
    M: 'static,
{
    let Some(filter) = method_filter(C::ROUTE.method) else {
        tracing::error!(method = %C::ROUTE.method, path = C::ROUTE.path, "this server version cannot serve this method: the route answers 405");
        return MethodRouter::new();
    };
    let router = axum::routing::on(filter, handler);
    if C::ROUTE.auth {
        router.route_layer(axum::middleware::from_fn(require_auth))
    } else {
        router
    }
}

/// `handler` mounted at `C::ROUTE` with the OpenAPI operation of `doc` (the output of
/// `utoipa_axum::routes!(handler)`; only its operation and schemas are used: the path and the
/// method in the document are always `C`'s). Use [`call_route!`](crate::call_route).
pub fn documented<C, H, T, M>(doc: UtoipaMethodRouter<AppState>, handler: H) -> UtoipaMethodRouter<AppState>
where
    C: HttpCall,
    H: CallHandler<C, T> + Handler<M, AppState>,
    M: 'static,
{
    let (schemas, doc_paths, _) = doc;
    let operation = doc_paths.paths.into_values().find_map(|item| item.get.or(item.put).or(item.post).or(item.delete).or(item.patch));
    let mut paths = Paths::new();
    if let Some(operation) = operation {
        paths.add_path_operation(C::ROUTE.path, vec![doc_method(C::ROUTE.method)], operation);
    }
    (schemas, paths, method_router::<C, H, T, M>(handler))
}

/// `handler` mounted at `C::ROUTE` on `router`, without OpenAPI documentation.
pub fn undocumented<C, H, T, M>(router: OpenApiRouter<AppState>, handler: H) -> OpenApiRouter<AppState>
where
    C: HttpCall,
    H: CallHandler<C, T> + Handler<M, AppState>,
    M: 'static,
{
    router.route(C::ROUTE.path, method_router::<C, H, T, M>(handler))
}

/// A documented typed route: `call_route!(WriteObject, put_object)` mounts `put_object` (which
/// carries `#[utoipa::path(..)]` and takes [`Call<WriteObject>`] last) at `WriteObject::ROUTE`.
/// Pass the result to [`NetBackendServer::routes`](crate::NetBackendServer::routes) or
/// `OpenApiRouter::routes`.
#[macro_export]
macro_rules! call_route {
    ($call:ty, $handler:path) => {
        $crate::http::call::documented::<$call, _, _, _>($crate::utoipa_axum::routes!($handler), $handler)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods() {
        for (method, filter, doc) in [
            (HttpMethod::Get, MethodFilter::GET, DocMethod::Get),
            (HttpMethod::Post, MethodFilter::POST, DocMethod::Post),
            (HttpMethod::Put, MethodFilter::PUT, DocMethod::Put),
            (HttpMethod::Patch, MethodFilter::PATCH, DocMethod::Patch),
            (HttpMethod::Delete, MethodFilter::DELETE, DocMethod::Delete),
        ] {
            assert_eq!(method_filter(method), Some(filter));
            assert!(supported_method(method));
            assert!(doc_method(method) == doc, "{method}");
        }
    }
}
