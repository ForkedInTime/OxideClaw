//! Minimal JSON-RPC 2.0 framing for ACP: one message per line.

use serde_json::{Value, json};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// ACP-reserved range starts at -32000; we use it for "turn already running".
pub const BUSY: i64 = -32000;

/// A decoded inbound message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// The client answering one of *our* requests (permission prompts).
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Decode one line. Lenient about a missing `"jsonrpc"` field; strict
/// about the request/notification/response shape.
pub fn parse(line: &str) -> Result<Incoming, RpcError> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| RpcError::new(PARSE_ERROR, format!("invalid JSON: {e}")))?;
    let Value::Object(mut obj) = v else {
        return Err(RpcError::new(INVALID_REQUEST, "expected a JSON object"));
    };
    let id = obj.remove("id").filter(|v| !v.is_null());
    let params = obj.remove("params").unwrap_or(Value::Null);
    if let Some(Value::String(method)) = obj.remove("method") {
        return Ok(match id {
            Some(id) => Incoming::Request { id, method, params },
            None => Incoming::Notification { method, params },
        });
    }
    let result = obj.remove("result");
    let error = obj.remove("error");
    match id {
        Some(id) if result.is_some() || error.is_some() => {
            Ok(Incoming::Response { id, result, error })
        }
        _ => Err(RpcError::new(
            INVALID_REQUEST,
            "expected a request, notification, or response",
        )),
    }
}

pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_has_an_id_and_a_method() {
        let m = parse(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            Incoming::Request {
                id: json!(0),
                method: "initialize".into(),
                params: json!({"protocolVersion": 1}),
            }
        );
    }

    #[test]
    fn string_ids_are_preserved_verbatim() {
        match parse(r#"{"jsonrpc":"2.0","id":"abc","method":"x"}"#).unwrap() {
            Incoming::Request { id, params, .. } => {
                assert_eq!(id, json!("abc"));
                assert_eq!(params, Value::Null);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_method_without_an_id_is_a_notification() {
        let m = parse(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#)
            .unwrap();
        assert_eq!(
            m,
            Incoming::Notification {
                method: "session/cancel".into(),
                params: json!({"sessionId": "s"}),
            }
        );
    }

    #[test]
    fn a_result_or_error_with_an_id_is_a_response() {
        let ok = parse(r#"{"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"cancelled"}}}"#)
            .unwrap();
        assert_eq!(
            ok,
            Incoming::Response {
                id: json!(7),
                result: Some(json!({"outcome": {"outcome": "cancelled"}})),
                error: None,
            }
        );
        let err = parse(r#"{"jsonrpc":"2.0","id":7,"error":{"code":-1,"message":"no"}}"#).unwrap();
        assert!(matches!(err, Incoming::Response { error: Some(_), .. }));
    }

    #[test]
    fn garbage_is_a_parse_error_and_a_bare_object_is_invalid() {
        assert_eq!(parse("{not json").unwrap_err().code, PARSE_ERROR);
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0"}"#).unwrap_err().code,
            INVALID_REQUEST
        );
        assert_eq!(parse(r#"[1,2]"#).unwrap_err().code, INVALID_REQUEST);
    }

    #[test]
    fn outbound_frames_carry_the_jsonrpc_marker() {
        assert_eq!(
            response(&json!(1), json!({"sessionId": "s"})),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"sessionId": "s"}})
        );
        assert_eq!(
            error(&json!("x"), METHOD_NOT_FOUND, "nope"),
            json!({"jsonrpc": "2.0", "id": "x", "error": {"code": -32601, "message": "nope"}})
        );
        assert_eq!(
            notification("session/update", json!({"a": 1})),
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"a": 1}})
        );
        assert_eq!(
            request(9, "session/request_permission", json!({})),
            json!({"jsonrpc": "2.0", "id": 9, "method": "session/request_permission", "params": {}})
        );
    }
}
