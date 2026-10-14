//! Wire framing + envelope helpers for the Wispr Flow helper IPC.
//!
//! Exact contract recovered from the shipped Electron bundle — see
//! `docs/reference/ipc-contract.md`. Summary:
//!   * body = `JSON.stringify(envelope, null, 2)` (pretty, 2-space)
//!   * escape: `+` -> `+1`, then `|` -> `+2`
//!   * frame  = escaped body + delimiter `'|'`
//!   * envelope = `{ "HelperAPIRequest": {..} }` or `{ "HelperAPIResponse": {..} }`
//!     where the inner object has exactly one command/response key plus `uuid`
//!     (and optional `sentryTrace` / `sentryBaggage`).

use serde_json::{json, Map, Value};

pub const DELIMITER: u8 = b'|';
/// Electron refuses to *send* a body longer than this; we keep ours under it too.
pub const MAX_BODY_LEN: usize = 30_000;

const META_KEYS: [&str; 3] = ["uuid", "sentryTrace", "sentryBaggage"];

/// Reverse of `escapeMessage`: `+2` -> `|`, then `+1` -> `+`. Order matters and
/// matches the JS `replaceAll` sequence exactly.
pub fn unescape(s: &str) -> String {
    s.replace("+2", "|").replace("+1", "+")
}

/// `escapeMessage`: `+` -> `+1`, then `|` -> `+2`.
pub fn escape(s: &str) -> String {
    s.replace('+', "+1").replace('|', "+2")
}

/// Streaming frame decoder. Feed raw bytes from stdin; yields complete,
/// unescaped JSON message bodies. Because escaping removes every literal `|`
/// from a body, splitting the stream on raw `'|'` is safe.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == DELIMITER {
                if !self.buf.is_empty() {
                    let escaped = String::from_utf8_lossy(&self.buf).into_owned();
                    out.push(unescape(&escaped));
                }
                self.buf.clear();
            } else {
                self.buf.push(b);
            }
        }
        out
    }
}

/// Encode an envelope `Value` into an on-the-wire frame (escaped body + `'|'`).
pub fn encode(envelope: &Value) -> Result<Vec<u8>, String> {
    // serde_json pretty printer uses 2-space indent, matching JSON.stringify(_, null, 2).
    let body = serde_json::to_string_pretty(envelope).map_err(|e| e.to_string())?;
    if body.len() > MAX_BODY_LEN {
        return Err(format!(
            "IPC message too long: {} > {}",
            body.len(),
            MAX_BODY_LEN
        ));
    }
    let mut frame = escape(&body).into_bytes();
    frame.push(DELIMITER);
    Ok(frame)
}

/// A decoded inbound message: which envelope kind, the command/response name,
/// its payload value (`Value::Bool(true)` for payload-less commands), and uuid.
#[derive(Debug)]
pub struct Incoming {
    pub kind: Kind,
    pub command: String,
    pub payload: Value,
    pub uuid: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Request,
    Response,
}

impl Incoming {
    pub fn parse(body: &str) -> Result<Incoming, String> {
        let v: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
        let obj = v.as_object().ok_or("envelope is not an object")?;
        let (kind, inner) = if let Some(r) = obj.get("HelperAPIRequest") {
            (Kind::Request, r)
        } else if let Some(r) = obj.get("HelperAPIResponse") {
            (Kind::Response, r)
        } else {
            return Err("envelope has neither HelperAPIRequest nor HelperAPIResponse".into());
        };
        let inner = inner.as_object().ok_or("inner is not an object")?;
        let uuid = inner
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // command = first non-metadata, non-null key
        let (command, payload) = inner
            .iter()
            .find(|(k, val)| !META_KEYS.contains(&k.as_str()) && !val.is_null())
            .map(|(k, val)| (k.clone(), val.clone()))
            .ok_or("no command key found in envelope")?;
        Ok(Incoming {
            kind,
            command,
            payload,
            uuid,
        })
    }
}

/// Build a `HelperAPIResponse` envelope: `{ HelperAPIResponse: { <name>: <body>, uuid } }`.
/// The inner object permits multiple known keys; use [`envelope`] for that.
pub fn response(name: &str, body: Value, uuid: &str) -> Value {
    let mut inner = Map::new();
    inner.insert(name.to_string(), body);
    envelope("HelperAPIResponse", inner, uuid)
}

/// Build a `HelperAPIRequest` envelope (helper -> Electron events, e.g. `AppInfoUpdate`
/// focus-change events, `ClipboardChanged`).
pub fn request(name: &str, body: Value, uuid: &str) -> Value {
    let mut inner = Map::new();
    inner.insert(name.to_string(), body);
    envelope("HelperAPIRequest", inner, uuid)
}

/// Wrap a pre-built inner object (one or more known keys) in an envelope with `uuid`.
/// Built explicitly as `Value::Object` (no reliance on `json!` coercion of a `Map`).
pub fn envelope(kind: &str, mut inner: Map<String, Value>, uuid: &str) -> Value {
    inner.insert("uuid".to_string(), Value::String(uuid.to_string()));
    let mut outer = Map::new();
    outer.insert(kind.to_string(), Value::Object(inner));
    Value::Object(outer)
}

/// `{ ACK: true, uuid }` — the readiness/keepalive reply.
pub fn ack(uuid: &str) -> Value {
    response("ACK", json!(true), uuid)
}

/// `{ HelperAPIError: { payload: { type, description, params } }, uuid }`.
pub fn error(description: &str, uuid: &str) -> Value {
    response(
        "HelperAPIError",
        json!({ "payload": { "type": "", "description": description, "params": {} } }),
        uuid,
    )
}

/// `PasteOutcome` event (helper -> Electron) reporting how a `PasteText` went:
/// `{ HelperAPIRequest: { PasteOutcome: { payload: { pasteId?, success, timeElapsedMs } }, uuid } }`.
/// `pasteId` is optional in the app's schema, which rejects a present-but-null
/// optional, so it is omitted rather than sent as null when the request had none.
pub fn paste_outcome(paste_id: Option<&str>, success: bool, elapsed_ms: u64, uuid: &str) -> Value {
    let mut payload = Map::new();
    if let Some(id) = paste_id {
        payload.insert("pasteId".to_string(), json!(id));
    }
    payload.insert("success".to_string(), json!(success));
    payload.insert("timeElapsedMs".to_string(), json!(elapsed_ms));
    request("PasteOutcome", json!({ "payload": payload }), uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_roundtrip() {
        for s in [
            "plain",
            "has|pipe",
            "has+plus",
            "a+b|c",
            "++||",
            "json {\"k\": \"a|b+c\"}",
        ] {
            assert_eq!(unescape(&escape(s)), s, "roundtrip failed for {s:?}");
        }
    }

    // `PasteOutcome` is decoded by the app's quicktype validator; pin the shape.
    #[test]
    fn paste_outcome_builds_paste_outcome_request() {
        let ev = paste_outcome(Some("paste-1"), true, 42, "po-1");
        let p = &ev["HelperAPIRequest"]["PasteOutcome"]["payload"];
        assert_eq!(p["pasteId"], "paste-1");
        assert_eq!(p["success"], true);
        assert_eq!(p["timeElapsedMs"], 42);
        assert_eq!(ev["HelperAPIRequest"]["uuid"], "po-1");
    }

    #[test]
    fn paste_outcome_omits_absent_paste_id() {
        let ev = paste_outcome(None, false, 7, "po-2");
        let p = &ev["HelperAPIRequest"]["PasteOutcome"]["payload"];
        assert!(
            p.get("pasteId").is_none(),
            "absent pasteId must be omitted, not null"
        );
        assert_eq!(p["success"], false);
    }

    #[test]
    fn escaped_body_has_no_literal_delimiter() {
        let e = escape("a|b|c+d");
        assert!(
            !e.contains('|'),
            "escaped body must contain no literal '|': {e}"
        );
    }

    #[test]
    fn decode_two_frames() {
        let f1 = escape(r#"{"HelperAPIRequest":{"IsReady":true,"uuid":"x"}}"#);
        let f2 = escape(r#"{"HelperAPIRequest":{"GetActiveAppInfo":true,"uuid":"y"}}"#);
        let mut d = FrameDecoder::default();
        let mut stream = f1.into_bytes();
        stream.push(DELIMITER);
        stream.extend(f2.into_bytes());
        stream.push(DELIMITER);
        let msgs = d.feed(&stream);
        assert_eq!(msgs.len(), 2);
        let a = Incoming::parse(&msgs[0]).unwrap();
        assert_eq!(a.kind, Kind::Request);
        assert_eq!(a.command, "IsReady");
        assert_eq!(a.uuid, "x");
        let b = Incoming::parse(&msgs[1]).unwrap();
        assert_eq!(b.command, "GetActiveAppInfo");
        assert_eq!(b.uuid, "y");
    }

    #[test]
    fn split_across_feeds() {
        let body = escape(r#"{"HelperAPIRequest":{"IsReady":true,"uuid":"z"}}"#).into_bytes();
        let (a, b) = body.split_at(10);
        let mut d = FrameDecoder::default();
        assert!(d.feed(a).is_empty());
        let mut rest = b.to_vec();
        rest.push(DELIMITER);
        let msgs = d.feed(&rest);
        assert_eq!(msgs.len(), 1);
        assert_eq!(Incoming::parse(&msgs[0]).unwrap().command, "IsReady");
    }
}
