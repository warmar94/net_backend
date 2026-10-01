//! The AsyncAPI 3.0 document of the WebSocket endpoint (`/v1/asyncapi.json`): generated at build
//! time from the registered handlers, so a game's own kinds (and their schemas, when documented)
//! appear without a hand-kept file. The envelope, authentication and close codes are fixed here.

use serde_json::{json, Map, Value};

use super::handlers::{HandlerMap, KindDocData};
use crate::config::Config;
use crate::http::middleware::MIN_PROTOCOL_VERSION;
use net_backend_protocol::{routes, PROTOCOL_HEADER, PROTOCOL_VERSION};

/// The protocol's close codes (`CloseCode::ALL`): (code, name, meaning, the client reconnects).
pub(crate) const CLOSE_CODES: &[(u16, &str, &str, bool)] = &[
    (1000, "normal", "normal closure", true),
    (1001, "going_away", "the server is shutting down or redeploying; reconnect", true),
    (1008, "policy_violation", "no `auth` in time, or too many messages", true),
    (1009, "message_too_big", "a message over the size limit", true),
    (1011, "internal_error", "an unexpected server error; reconnect", true),
    (1013, "try_again_later", "overloaded, or this connection could not keep up with its pushes; reconnect later and resync", true),
    (4001, "unauthorized", "authentication refused or revoked (logout, password change, admin): log in again", false),
    (4003, "banned", "the account is banned", false),
    (4009, "replaced", "replaced by a newer connection (the user opened more connections than the server allows)", false),
    (4010, "unsupported_protocol", "the client's protocol version is not supported", false),
];

/// A message name usable as a JSON-pointer-free key (`[A-Za-z0-9._-]`).
fn key(kind: &str) -> String {
    kind.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
}

fn any_json() -> Value {
    json!({ "description": "any JSON value" })
}

fn api_error_ref() -> Value {
    json!({ "$ref": "#/components/schemas/ApiError" })
}

fn message(name: &str, title: &str, summary: Option<&str>, description: Option<&str>, payload: Value, example: Value) -> Value {
    let mut object = Map::new();
    object.insert("name".into(), json!(name));
    object.insert("title".into(), json!(title));
    if let Some(summary) = summary {
        object.insert("summary".into(), json!(summary));
    }
    if let Some(description) = description {
        object.insert("description".into(), json!(description));
    }
    object.insert("contentType".into(), json!("application/json"));
    object.insert("payload".into(), payload);
    object.insert("examples".into(), json!([{ "payload": example }]));
    Value::Object(object)
}

fn request_payload(kind: &str, data: Value) -> Value {
    json!({
        "type": "object",
        "required": ["id", "type"],
        "properties": {
            "id": { "type": "integer", "minimum": 0, "description": "unsigned 64-bit request id, unique per client process, echoed in the answer" },
            "type": { "const": kind },
            "data": data,
        },
    })
}

fn answer_payload(data: Value) -> Value {
    json!({
        "type": "object",
        "required": ["id", "ok"],
        "properties": {
            "id": { "type": "integer", "minimum": 0 },
            "ok": { "const": true },
            "data": data,
        },
    })
}

fn push_payload(kind: &str, data: Value) -> Value {
    json!({
        "type": "object",
        "required": ["type"],
        "properties": { "type": { "const": kind }, "data": data },
        "not": { "anyOf": [{ "required": ["ok"] }, { "required": ["id"] }] },
    })
}

fn description(config: &Config) -> String {
    let ws = &config.ws;
    let mut text = format!(
        "The WebSocket endpoint `{path}`: JSON objects in text frames.\n\n\
         **Frames.** Request `{{\"id\":7,\"type\":\"<kind>\",\"data\":{{…}}}}` → answer `{{\"id\":7,\"ok\":true,\"data\":{{…}}}}` or \
         `{{\"id\":7,\"ok\":false,\"error\":{{\"code\":\"…\",\"message\":\"…\"}}}}`. Push `{{\"type\":\"<kind>\",\"data\":{{…}}}}` (never `id` or `ok`). \
         A frame with a numeric `id` and (an `ok` field or no `type`) is an answer. Every request gets exactly one answer; \
         a malformed frame that has an unsigned-integer `id` is answered `bad_request`, one without is dropped. An unknown \
         `type` is answered `unknown_type`. Binary frames are not part of the protocol.\n\n\
         **Authentication.** Either `Authorization: Bearer <access token>` on the handshake (or `?token=` when enabled: {query}), or \
         the first message `{{\"type\":\"auth\",\"data\":{{\"token\":\"…\",\"protocol\":{version}}}}}` within {auth} s (then close 1008). A bad \
         handshake token is HTTP 401 (`token_expired` for an expired one: refresh, then reconnect); a banned account 403. Every \
         `auth` gets exactly one `auth.ok` or `auth.failed` (then close 4001, 4003 when banned, 4010 for an unsupported \
         `protocol`); a later `auth` with the same user's token re-authenticates; another user's token is refused. An open \
         connection survives the expiry of its access token; revoking the session closes it (4001; a ban 4003).\n\n\
         **Versions.** The client may name its protocol version in the `{header}` handshake header or the `protocol` field of \
         `auth` (absent = 1). Supported: {min}..={max}. An unsupported one: upgrade, then close 4010 (or HTTP 403 when the \
         request is not an upgrade). This endpoint never answers HTTP 400.\n\n\
         **Handshake answers.** 101 (upgraded); 401 `unauthorized` / `token_expired` (refresh, then reconnect); 403 `banned` or a server rule; 426 without an upgrade; 429 + `Retry-After` (too many handshakes or sockets from one address); 503 + `Retry-After` (full, too many sockets waiting for `auth`, shutting down, a temporary error). For first-message `auth`, a temporary server error closes with 1013 WITHOUT `auth.failed` (reconnect and retry).\n\n\
         **Limits.** Messages up to {max_bytes} bytes (bigger: close 1009); {fps} frames per second per connection with a \
         burst of {burst} (over it: `rate_limited`, close 1008 when flooding); {per_user} connections per user (the oldest \
         is closed with 4009); {rooms} rooms per connection. The server pings every {ping} s and drops a connection that \
         sent nothing for {idle} s; it answers the client's pings.\n\n**Close codes.**\n\n| Code | Meaning | Reconnect |\n|---|---|---|\n",
        path = routes::WS,
        query = if ws.query_token { "on" } else { "off" },
        version = PROTOCOL_VERSION,
        auth = ws.auth_timeout_secs,
        header = PROTOCOL_HEADER,
        min = MIN_PROTOCOL_VERSION,
        max = PROTOCOL_VERSION,
        max_bytes = ws.max_message_bytes,
        fps = ws.frames_per_second,
        burst = ws.frame_burst,
        per_user = ws.max_connections_per_user,
        rooms = ws.max_rooms_per_connection,
        ping = ws.ping_interval_secs,
        idle = ws.idle_timeout_secs,
    );
    for (code, _, meaning, reconnect) in CLOSE_CODES {
        text.push_str(&format!("| {code} | {meaning} | {} |\n", if *reconnect { "yes" } else { "no" }));
    }
    text
}

/// A message key for `kind` not used yet (`key`, then `key_2`, `key_3`…: `a:b` and `a_b` both map
/// to `a_b`).
fn unique_key(kind: &str, used: &mut std::collections::HashSet<String>) -> String {
    let base = key(kind);
    let mut candidate = base.clone();
    let mut n = 2;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}_{n}");
        n += 1;
    }
    candidate
}

fn kind_messages(kind: &str, key: &str, doc: &KindDocData) -> (Value, Value) {
    let data = doc.request.clone().unwrap_or_else(any_json);
    let answer = doc.response.clone().unwrap_or_else(any_json);
    let request = message(
        &format!("request.{key}"),
        &format!("{kind} (request)"),
        doc.summary.as_deref(),
        doc.description.as_deref(),
        request_payload(kind, data),
        json!({ "id": 7, "type": kind, "data": {} }),
    );
    let reply = message(&format!("answer.{key}"), &format!("{kind} (answer)"), None, None, answer_payload(answer), json!({ "id": 7, "ok": true, "data": {} }));
    (request, reply)
}

/// The document as JSON text.
pub(crate) fn document(config: &Config, handlers: &HandlerMap) -> String {
    let channel = json!({ "$ref": "#/channels/ws" });
    let mut messages = Map::new();
    let mut channel_messages = Map::new();
    let mut operations = Map::new();
    let add = |name: String, value: Value, messages: &mut Map<String, Value>, channel_messages: &mut Map<String, Value>| {
        channel_messages.insert(name.clone(), json!({ "$ref": format!("#/components/messages/{name}") }));
        messages.insert(name, value);
    };
    let msg_ref = |name: &str| json!({ "$ref": format!("#/channels/ws/messages/{name}") });

    add(
        "auth".into(),
        message(
            "auth",
            "auth (first-message authentication)",
            Some("Authenticate the connection (client → server, no `id`)"),
            None,
            json!({
                "type": "object",
                "required": ["type", "data"],
                "properties": {
                    "type": { "const": "auth" },
                    "data": {
                        "type": "object",
                        "required": ["token"],
                        "properties": { "token": { "type": "string", "description": "the access token" }, "protocol": { "type": "integer", "minimum": 1 } },
                    },
                },
            }),
            json!({ "type": "auth", "data": { "token": "nbsa_…", "protocol": PROTOCOL_VERSION } }),
        ),
        &mut messages,
        &mut channel_messages,
    );
    add(
        "auth.ok".into(),
        message(
            "auth.ok",
            "auth.ok",
            Some("The `auth` message was accepted"),
            None,
            json!({
                "type": "object",
                "required": ["type"],
                "properties": {
                    "type": { "const": "auth.ok" },
                    "data": { "type": "object", "properties": { "user_id": { "type": "integer" }, "protocol": { "type": "integer" } } },
                },
            }),
            json!({ "type": "auth.ok", "data": { "user_id": 42, "protocol": PROTOCOL_VERSION } }),
        ),
        &mut messages,
        &mut channel_messages,
    );
    add(
        "auth.failed".into(),
        message(
            "auth.failed",
            "auth.failed",
            Some("The `auth` message was refused; the server closes the connection (4001, 4003 banned, 4010 version)"),
            None,
            json!({ "type": "object", "required": ["type", "error"], "properties": { "type": { "const": "auth.failed" }, "error": api_error_ref() } }),
            json!({ "type": "auth.failed", "error": { "code": "unauthorized", "message": "the access token is invalid or revoked" } }),
        ),
        &mut messages,
        &mut channel_messages,
    );
    add(
        "error".into(),
        message(
            "error",
            "error answer",
            Some("A refused request (any kind)"),
            Some("Codes: `bad_request` (malformed frame or data), `unauthorized` (not authenticated yet), `unknown_type`, `rate_limited` (`details.retry_after_ms`), `payload_too_large`, `unavailable`, `internal`, plus each kind's own codes."),
            json!({
                "type": "object",
                "required": ["id", "ok", "error"],
                "properties": { "id": { "type": "integer", "minimum": 0 }, "ok": { "const": false }, "error": api_error_ref() },
            }),
            json!({ "id": 7, "ok": false, "error": { "code": "unknown_type", "message": "unknown request type" } }),
        ),
        &mut messages,
        &mut channel_messages,
    );
    operations.insert(
        "authenticate".into(),
        json!({
            "action": "receive",
            "channel": channel,
            "summary": "First-message authentication",
            "messages": [msg_ref("auth")],
            "reply": { "channel": channel, "messages": [msg_ref("auth.ok"), msg_ref("auth.failed")] },
        }),
    );
    let mut used = std::collections::HashSet::new();
    for (kind, entry) in &handlers.kinds {
        let key = unique_key(kind, &mut used);
        let (request, reply) = kind_messages(kind, &key, &entry.doc);
        let request_name = format!("request.{key}");
        let answer_name = format!("answer.{key}");
        add(request_name.clone(), request, &mut messages, &mut channel_messages);
        add(answer_name.clone(), reply, &mut messages, &mut channel_messages);
        let mut operation = json!({
            "action": "receive",
            "channel": channel,
            "messages": [msg_ref(&request_name)],
            "reply": { "channel": channel, "messages": [msg_ref(&answer_name), msg_ref("error")] },
        });
        if let (Some(summary), Some(object)) = (&entry.doc.summary, operation.as_object_mut()) {
            object.insert("summary".into(), json!(summary));
        }
        operations.insert(request_name.clone(), operation);
    }
    let mut used = std::collections::HashSet::new();
    for (kind, doc) in &handlers.pushes {
        let name = format!("push.{}", unique_key(kind, &mut used));
        let data = doc.request.clone().unwrap_or_else(any_json);
        add(
            name.clone(),
            message(
                &name,
                &format!("{kind} (push)"),
                doc.summary.as_deref(),
                doc.description.as_deref(),
                push_payload(kind, data),
                json!({ "type": kind, "data": {} }),
            ),
            &mut messages,
            &mut channel_messages,
        );
        operations.insert(name.clone(), json!({ "action": "send", "channel": channel, "messages": [msg_ref(&name)] }));
    }
    let mut schemas = Map::new();
    schemas.insert(
        "ApiError".into(),
        json!({
            "type": "object",
            "required": ["code"],
            "properties": {
                "code": { "type": "string", "description": "stable snake_case code; branch on it" },
                "message": { "type": "string", "description": "human-readable English; may change" },
                "details": { "description": "code-specific JSON" },
            },
        }),
    );
    for (name, schema) in &handlers.schemas {
        schemas.insert(name.clone(), schema.clone());
    }
    let close_codes: Vec<Value> =
        CLOSE_CODES.iter().map(|(code, name, meaning, reconnect)| json!({ "code": code, "name": name, "meaning": meaning, "reconnect": reconnect })).collect();
    let document = json!({
        "asyncapi": "3.0.0",
        "info": {
            "title": format!("{} (WebSocket)", config.openapi.title),
            "version": config.openapi.version,
            "description": description(config),
        },
        "defaultContentType": "application/json",
        "channels": {
            "ws": {
                "address": routes::WS,
                "title": "The WebSocket connection",
                "messages": channel_messages,
                "bindings": {
                    "ws": {
                        "method": "GET",
                        "headers": {
                            "type": "object",
                            "properties": {
                                "Authorization": { "type": "string", "description": "Bearer <access token>" },
                                PROTOCOL_HEADER: { "type": "string", "description": "the client's protocol version (absent = 1)" },
                            },
                        },
                        "query": { "type": "object", "properties": {
                            "token": { "type": "string", "description": "the access token (only when the server enables ws.query_token)" },
                            "access_token": { "type": "string", "description": "the same, other name" },
                        } },
                        "bindingVersion": "0.1.0",
                    },
                },
            },
        },
        "operations": operations,
        "components": { "messages": messages, "schemas": schemas },
        "x-close-codes": close_codes,
    });
    serde_json::to_string(&document).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::WsHandlers;

    /// Every `$ref` points at something that exists.
    fn check_refs(value: &Value, root: &Value) {
        match value {
            Value::Object(object) => {
                if let Some(Value::String(target)) = object.get("$ref") {
                    let pointer = target.strip_prefix('#').unwrap_or(target);
                    // JSON pointers escape `/` as `~1`; our names contain none.
                    assert!(root.pointer(pointer).is_some(), "dangling $ref {target}");
                }
                object.values().for_each(|v| check_refs(v, root));
            }
            Value::Array(items) => items.iter().for_each(|v| check_refs(v, root)),
            _ => {}
        }
    }

    #[test]
    fn document_lists_kinds_pushes_and_close_codes() {
        let mut handlers = WsHandlers::new();
        handlers.raw("game.echo", |_, data| async move { Ok(data) }).summary("Echo").data_schema(json!({"type":"object"}));
        handlers.raw("lobby:join", |_, _| async { Ok(Value::Null) });
        handlers.raw("lobby_join", |_, _| async { Ok(Value::Null) });
        handlers.push_kind("game.tick").summary("A tick");
        let map = handlers.finish().unwrap_or_default();
        let text = document(&Config::default(), &map);
        let doc: Value = serde_json::from_str(&text).unwrap_or_default();
        assert_eq!(doc["asyncapi"], "3.0.0");
        assert_eq!(doc["channels"]["ws"]["address"], "/v1/ws");
        for name in
            ["auth", "auth.ok", "auth.failed", "error", "request.game.echo", "answer.game.echo", "request.lobby_join", "request.lobby_join_2", "push.game.tick"]
        {
            assert!(doc["components"]["messages"].get(name).is_some(), "{name}");
            assert!(doc["channels"]["ws"]["messages"].get(name).is_some(), "{name}");
        }
        assert_eq!(doc["operations"]["request.game.echo"]["summary"], "Echo");
        assert_eq!(doc["operations"]["push.game.tick"]["action"], "send");
        assert_eq!(doc["x-close-codes"].as_array().map(Vec::len), Some(CLOSE_CODES.len()));
        assert!(doc["info"]["description"].as_str().is_some_and(|d| d.contains("| 4010 |") && d.contains("never answers HTTP 400")));
        check_refs(&doc, &doc);
        // Every close code the protocol names is documented.
        for code in net_backend_protocol::CloseCode::ALL {
            assert!(CLOSE_CODES.iter().any(|(c, ..)| *c == code.get()), "{code}");
        }
    }
}
