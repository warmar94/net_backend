//! WebSocket request handlers by kind, and their documentation for the AsyncAPI document.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use net_backend_protocol::{kinds, ServerPush, UserId, WsCall};
use serde_json::Value;

use super::hub::{ConnectionId, Hub};
use crate::auth::AuthContext;
use crate::error::AppError;
use crate::state::AppState;

/// The longest request / push kind, in bytes.
pub const MAX_KIND_BYTES: usize = 64;

/// What a WebSocket handler receives besides the request: the app state, the socket and who is
/// calling.
#[derive(Clone)]
#[non_exhaustive]
pub struct WsCtx {
    /// The app state (database, config, the game's own state values, [`AppState::ws`]).
    pub state: AppState,
    /// The socket the request came on.
    pub connection: ConnectionId,
    /// Who is calling (the socket is always authenticated when a handler runs). `roles` are the
    /// roles as last known: refreshed at once for changes made in this process, within
    /// `ws.roles_refresh_secs` for changes made elsewhere (the command line, another instance).
    pub auth: AuthContext,
    /// The request's `id`.
    pub request_id: u64,
    /// The request's kind.
    pub kind: Arc<str>,
}

impl fmt::Debug for WsCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsCtx")
            .field("connection", &self.connection)
            .field("user", &self.auth.user_id)
            .field("request_id", &self.request_id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl WsCtx {
    /// The hub (pushes, rooms).
    pub fn hub(&self) -> &Hub {
        self.state.ws()
    }

    /// The calling user.
    pub fn user_id(&self) -> UserId {
        self.auth.user_id
    }

    /// A context for unit-testing a handler outside a socket (connection id 0, request id 1).
    #[doc(hidden)]
    pub fn for_tests(state: AppState, auth: AuthContext, kind: &str) -> Self {
        Self { state, connection: ConnectionId::for_tests(0), auth, request_id: 1, kind: Arc::from(kind) }
    }
}

pub(crate) type HandlerFn = Arc<dyn Fn(WsCtx, Value) -> BoxFuture<'static, Result<Value, AppError>> + Send + Sync>;

/// The documentation of one kind.
#[derive(Clone, Debug, Default)]
pub(crate) struct KindDocData {
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    /// The request's `data` (or the push's `data`).
    pub(crate) request: Option<Value>,
    /// The answer's `data`.
    pub(crate) response: Option<Value>,
}

pub(crate) struct HandlerEntry {
    pub(crate) handler: HandlerFn,
    pub(crate) doc: KindDocData,
}

/// The frozen handler table the hub dispatches with, plus the documented pushes and schemas.
#[derive(Default)]
pub(crate) struct HandlerMap {
    pub(crate) kinds: BTreeMap<String, HandlerEntry>,
    pub(crate) pushes: BTreeMap<String, KindDocData>,
    pub(crate) schemas: BTreeMap<String, Value>,
}

impl HandlerMap {
    pub(crate) fn get(&self, kind: &str) -> Option<&HandlerFn> {
        self.kinds.get(kind).map(|e| &e.handler)
    }

    pub(crate) fn len(&self) -> usize {
        self.kinds.len()
    }
}

/// The WebSocket request handlers of the app and its modules, by kind.
///
/// Register with [`call`](Self::call) (typed, through the protocol's [`WsCall`]) or
/// [`raw`](Self::raw) (any JSON); document server pushes with [`push`](Self::push) /
/// [`push_kind`](Self::push_kind). Each returns a [`KindDoc`] for the AsyncAPI document. A kind may
/// be registered once; `auth`, `auth.ok` and `auth.failed` are reserved.
#[derive(Default)]
pub struct WsHandlers {
    map: HandlerMap,
    owner: &'static str,
    problems: Vec<String>,
    owners: BTreeMap<String, &'static str>,
}

impl fmt::Debug for WsHandlers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsHandlers").field("kinds", &self.map.kinds.keys().collect::<Vec<_>>()).finish_non_exhaustive()
    }
}

/// Check a request / push kind: 1–64 bytes of `[a-z0-9_.:-]`, starting with a letter, not an
/// auth kind.
pub(crate) fn validate_kind(kind: &str) -> Result<(), String> {
    if kinds::is_reserved(kind) {
        return Err(format!("WebSocket kind `{kind}` is reserved for authentication"));
    }
    let first_ok = kind.bytes().next().is_some_and(|b| b.is_ascii_lowercase());
    if !first_ok || kind.len() > MAX_KIND_BYTES || !kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b':' | b'-'))
    {
        return Err(format!("WebSocket kind `{kind}` must be 1-{MAX_KIND_BYTES} bytes of [a-z0-9_.:-] starting with a letter"));
    }
    Ok(())
}

impl WsHandlers {
    pub(crate) fn new() -> Self {
        Self { owner: "app", ..Self::default() }
    }

    pub(crate) fn set_owner(&mut self, owner: &'static str) {
        self.owner = owner;
    }

    /// Every registration problem (bad or duplicate kinds), or the frozen table.
    pub(crate) fn finish(self) -> Result<HandlerMap, Vec<String>> {
        if self.problems.is_empty() {
            Ok(self.map)
        } else {
            Err(self.problems)
        }
    }

    fn insert(&mut self, kind: &str, handler: HandlerFn) -> KindDoc<'_> {
        if let Err(problem) = validate_kind(kind) {
            self.problems.push(problem);
            return KindDoc { doc: None, schemas: &mut self.map.schemas };
        }
        if let Some(first) = self.owners.get(kind) {
            self.problems.push(format!("WebSocket kind `{kind}` is registered twice (by `{first}` and `{}`)", self.owner));
            return KindDoc { doc: None, schemas: &mut self.map.schemas };
        }
        self.owners.insert(kind.to_string(), self.owner);
        let entry = self.map.kinds.entry(kind.to_string()).or_insert(HandlerEntry { handler, doc: KindDocData::default() });
        KindDoc { doc: Some(&mut entry.doc), schemas: &mut self.map.schemas }
    }

    /// A typed handler for `C::KIND`: the request's `data` decodes as `C` (else the client gets
    /// `bad_request`), the handler's `Ok` is the answer's `data`.
    pub fn call<C, F, Fut>(&mut self, handler: F) -> KindDoc<'_>
    where
        C: WsCall + Send + 'static,
        F: Fn(WsCtx, C) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<C::Response, AppError>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let wrapped: HandlerFn = Arc::new(move |ctx: WsCtx, data: Value| {
            let handler = handler.clone();
            Box::pin(async move {
                // Never quote the payload back (it may hold anything); the kind is enough.
                let request = serde_json::from_value::<C>(data).map_err(|_| AppError::bad_request("the request data is malformed"))?;
                let response = handler(ctx, request).await?;
                serde_json::to_value(&response).map_err(AppError::internal)
            })
        });
        self.insert(C::KIND, wrapped)
    }

    /// An untyped handler for `kind`: gets the request's `data` as JSON, answers JSON.
    pub fn raw<F, Fut>(&mut self, kind: &str, handler: F) -> KindDoc<'_>
    where
        F: Fn(WsCtx, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, AppError>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let wrapped: HandlerFn = Arc::new(move |ctx: WsCtx, data: Value| {
            let handler = handler.clone();
            Box::pin(async move { handler(ctx, data).await })
        });
        self.insert(kind, wrapped)
    }

    /// Document a server push `P::KIND` (pushes need no registration to be sent; this only puts
    /// them into the AsyncAPI document).
    pub fn push<P: ServerPush>(&mut self) -> KindDoc<'_> {
        self.push_kind(P::KIND)
    }

    /// Document a server push by kind.
    pub fn push_kind(&mut self, kind: &str) -> KindDoc<'_> {
        if let Err(problem) = validate_kind(kind) {
            self.problems.push(problem);
            return KindDoc { doc: None, schemas: &mut self.map.schemas };
        }
        let doc = self.map.pushes.entry(kind.to_string()).or_default();
        KindDoc { doc: Some(doc), schemas: &mut self.map.schemas }
    }

    /// The registered request kinds, sorted.
    pub fn kinds(&self) -> Vec<&str> {
        self.map.kinds.keys().map(String::as_str).collect()
    }
}

/// Documentation of a kind for the AsyncAPI document (`/v1/asyncapi.json`). Every method is
/// optional; an undocumented kind still appears, with any JSON as its data.
pub struct KindDoc<'a> {
    doc: Option<&'a mut KindDocData>,
    schemas: &'a mut BTreeMap<String, Value>,
}

impl fmt::Debug for KindDoc<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KindDoc").field("doc", &self.doc).finish_non_exhaustive()
    }
}

fn schema_of<T: utoipa::ToSchema>(schemas: &mut BTreeMap<String, Value>) -> Value {
    let mut nested = Vec::new();
    T::schemas(&mut nested);
    for (name, schema) in nested {
        if let Ok(value) = serde_json::to_value(&schema) {
            schemas.insert(name, value);
        }
    }
    serde_json::to_value(T::schema()).unwrap_or(Value::Null)
}

impl KindDoc<'_> {
    /// A one-line summary.
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.summary = Some(summary.into());
        }
        self
    }

    /// A longer description (Markdown).
    pub fn description(mut self, description: impl Into<String>) -> Self {
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.description = Some(description.into());
        }
        self
    }

    /// The JSON Schema of the request's `data` (of a push: its `data`).
    pub fn data_schema(mut self, schema: Value) -> Self {
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.request = Some(schema);
        }
        self
    }

    /// The JSON Schema of the answer's `data`.
    pub fn answer_schema(mut self, schema: Value) -> Self {
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.response = Some(schema);
        }
        self
    }

    /// Both schemas from types deriving `utoipa::ToSchema` (re-exported as
    /// `net_backend_server::utoipa`); nested schemas go into the document's components.
    pub fn schemas<Req: utoipa::ToSchema, Res: utoipa::ToSchema>(mut self) -> Self {
        let request = schema_of::<Req>(self.schemas);
        let response = schema_of::<Res>(self.schemas);
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.request = Some(request);
            doc.response = Some(response);
        }
        self
    }

    /// A push's `data` schema from a type deriving `utoipa::ToSchema`.
    pub fn schema<T: utoipa::ToSchema>(mut self) -> Self {
        let schema = schema_of::<T>(self.schemas);
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.request = Some(schema);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_checked() {
        for ok in ["chat.send", "game.x-y", "a", "lobby:join", "m2.do_it"] {
            assert!(validate_kind(ok).is_ok(), "{ok}");
        }
        for bad in ["", "auth", "auth.ok", "auth.failed", "Chat.send", "1x", "a b", ".x", &"x".repeat(65)] {
            assert!(validate_kind(bad).is_err(), "{bad}");
        }
        let mut handlers = WsHandlers::new();
        handlers.raw("game.ping", |_, _| async { Ok(Value::Null) }).summary("ping");
        handlers.set_owner("chat");
        handlers.raw("game.ping", |_, _| async { Ok(Value::Null) });
        handlers.raw("auth", |_, _| async { Ok(Value::Null) });
        let problems = handlers.finish().err().unwrap_or_default();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("registered twice (by `app` and `chat`)"), "{problems:?}");
    }
}
