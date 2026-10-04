//! Feature `bevy_net_backend`: typed HTTP calls through that client. Any [`HttpCall`] (every
//! route of every server module, or a game's own) becomes a `bevy_net_backend` request with its
//! method, path, JSON body or query string and the protocol header, exactly as
//! `net_backend_client` sends it; the answer arrives as that client's `JsonResponse<C::Response>`,
//! and [`api_error`] reads the protocol's error out of a refused one.
//!
//! - [`request`]: the `OutgoingRequest` of a call, to adjust (a timeout, a header) and send with
//!   `HttpClient::send_json::<C::Response>`.
//! - [`HttpClientCalls::call`]: build and send in one step.
//! - [`api_error`]: the [`ApiError`] of a non-2xx answer (`BackendError::Status` whose body is the
//!   protocol's `{"error":{…}}`).
//!
//! A route that needs a Bearer token gets the game's `BackendCredentials` (an [`AccessToken`]
//! is one, see the crate's `Credentials` impl); a route that does not (login, registration,
//! refresh, …) is sent without them, as `net_backend_client` does. Logout takes either: it is
//! sent with the game's credentials (when the game has any), so `LogoutRequest::everywhere()`
//! works with the access token alone, as `net_backend_client`'s `logout` sends it. A path parameter that is
//! missing or would need escaping, or a query payload that is not a flat object, is answered
//! `InvalidRequest` and never sent.
//!
//! ```
//! use bevy_net_backend::{BackendError, HttpClient, JsonResponse};
//! use net_backend_protocol::auth::{Account, GetAccount};
//! use net_backend_protocol::bevy::{api_error, request, HttpClientCalls};
//! use net_backend_protocol::{codes, HttpCall};
//!
//! // Register the answer type once: `app.add_json_response::<Account>()`.
//! fn load_account(client: &HttpClient) {
//!     client.call(&GetAccount::new());
//! }
//!
//! fn read_answer(answer: &JsonResponse<Account>) {
//!     match &answer.result {
//!         Ok(account) => println!("signed in as account {}", account.id.get()),
//!         Err(error) => match api_error(error) {
//!             Some(api) if api.code == codes::UNAUTHORIZED => println!("log in again"),
//!             Some(api) => println!("the server refused: {}", api.message),
//!             None => println!("no answer from the server: {error}"),
//!         },
//!     }
//! }
//!
//! let built = request(&GetAccount::new());
//! assert_eq!((built.method().as_str(), built.path()), ("GET", GetAccount::ROUTE.path));
//! # let _ = (load_account, read_answer);
//! ```
//!
//! [`AccessToken`]: crate::AccessToken

use bevy_net_backend::http::Method;
use bevy_net_backend::{BackendError, HttpClient, OutgoingRequest, RequestId};

use crate::error::{ApiError, ErrorBody};
use crate::http_call::{query_pairs, HttpCall, PayloadKind};
use crate::version::{PROTOCOL_HEADER, PROTOCOL_VERSION};

/// The `bevy_net_backend` request for `call`: `C::ROUTE.method` on [`call.path()`](HttpCall::path),
/// the payload as the JSON body (`content-type: application/json`) or the query string (absent
/// fields left out), `accept: application/json` and [`PROTOCOL_HEADER`]; without the game's
/// credentials when the route needs no token (except logout, which takes a Bearer token or the
/// refresh token in the body: it keeps them). Send it with
/// `HttpClient::send_json::<C::Response>` (after `app.add_json_response::<C::Response>()`).
///
/// A path parameter that is missing or would need escaping, or a query payload that is not a
/// flat object: the request is marked invalid (answered `InvalidRequest`, never sent).
pub fn request<C: HttpCall>(call: &C) -> OutgoingRequest {
    let method = Method::from_bytes(C::ROUTE.method.as_str().as_bytes()).unwrap_or(Method::GET);
    let Some(path) = call.path() else {
        let mut refused = OutgoingRequest::new(method, "/");
        refused.reject(format!("a path parameter of `{}` is missing or would need escaping", C::ROUTE.path));
        return refused;
    };
    let mut request = OutgoingRequest::new(method, path).with_header("accept", "application/json").with_header(PROTOCOL_HEADER, &PROTOCOL_VERSION.to_string());
    match C::PAYLOAD {
        PayloadKind::Json => request = request.with_json(call.payload()),
        PayloadKind::Query => match query_pairs(call.payload()) {
            Ok(pairs) => {
                for (name, value) in pairs {
                    request = request.with_query(name, value);
                }
            }
            Err(error) => request.reject(error.message),
        },
        // `Empty` and any later kind without a payload on the wire.
        _ => {}
    }
    // Logout (`auth: false`) also takes a Bearer token instead of the refresh token in the body.
    if !C::ROUTE.auth && C::ROUTE.path != crate::routes::auth::LOGOUT {
        request = request.without_credentials();
    }
    request
}

/// The protocol's error in a refused answer: `Some` for a [`BackendError::Status`] whose body is
/// an [`ErrorBody`] (`{"error":{"code":…,"message":…}}`, every error of every route), `None` for
/// any other error (no answer, a proxy's error page, a timeout, …). The status is the
/// `BackendError`'s own (`BackendError::Status` carries it), and so is `retry_after()`.
pub fn api_error(error: &BackendError) -> Option<ApiError> {
    match error {
        BackendError::Status(raw) => serde_json::from_slice::<ErrorBody>(raw.body()).ok().map(|body| body.error),
        _ => None,
    }
}

/// Typed calls on `bevy_net_backend`'s [`HttpClient`].
pub trait HttpClientCalls {
    /// Send `call` ([`request`]) and decode a 2xx answer as `C::Response`: the answer arrives as
    /// `JsonResponse<C::Response>` (register it once with `app.add_json_response::<C::Response>()`;
    /// an unregistered type is answered on `HttpResponse` with `InvalidRequest`, never sent).
    fn call<C: HttpCall>(&self, call: &C) -> RequestId
    where
        C::Response: Send + Sync + 'static;
}

impl HttpClientCalls for HttpClient {
    fn call<C: HttpCall>(&self, call: &C) -> RequestId
    where
        C::Response: Send + Sync + 'static,
    {
        self.send_json::<C::Response>(request(call))
    }
}
