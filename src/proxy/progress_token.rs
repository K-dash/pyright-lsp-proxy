//! Namespacing for work-done progress tokens (`window/workDoneProgress/create`
//! params, `$/progress` notification params, and `window/workDoneProgress/cancel`
//! params).
//!
//! LSP's `ProgressToken` is `integer | string` — exactly the shape `RpcId`
//! already models, so it's reused here instead of introducing a new type.
//! Backend-originated tokens are rewritten to a session-prefixed string
//! before reaching the client, so two backends that happen to emit the same
//! raw token value (see #104) can never collide in the client's flat token
//! space. The encoding is stateless (a pure string transform, no side
//! table), so there is nothing to clean up on backend eviction/crash.

use crate::message::RpcId;
use serde_json::Value;

/// Rewrite a `window/workDoneProgress/create` or `$/progress` message's
/// `params.token` in place to a session-prefixed string, and return the
/// ORIGINAL token (for warmup-gating comparisons, which must never compare
/// against the rewritten value). Returns `None` — leaving `msg` untouched —
/// if `params.token` is missing or not an `integer | string`, which is a
/// backend protocol violation; callers log this rather than silently
/// swallowing the message.
pub(crate) fn namespace(params: &mut Value, session: u64) -> Option<RpcId> {
    let original = extract(Some(params))?;
    let obj = params.as_object_mut()?;
    obj.insert(
        "token".to_string(),
        Value::String(encode(session, &original)),
    );
    Some(original)
}

/// Extract `params.token` as an `RpcId`, without mutating anything.
pub(crate) fn extract(params: Option<&Value>) -> Option<RpcId> {
    let token = params?.get("token")?;
    serde_json::from_value(token.clone()).ok()
}

/// Encode a session id and an original token into the proxy-namespaced
/// string sent to the client: `tmx:{session}:{n|s}:{original}`. The `n`/`s`
/// tag records the original JSON type, so `decode` can restore it losslessly
/// (a bare string encoding alone couldn't distinguish token `5` (integer)
/// from token `"5"` (string) on the way back).
fn encode(session: u64, token: &RpcId) -> String {
    match token {
        RpcId::Number(n) => format!("tmx:{session}:n:{n}"),
        RpcId::String(s) => format!("tmx:{session}:s:{s}"),
    }
}

/// Reverse `encode`: split a client-presented prefixed token back into its
/// owning session id and original (type-preserved) token. Returns `None` if
/// `raw` isn't a proxy-namespaced token (e.g. a backend that was never
/// routed through `namespace`, or a client typo) — callers drop the request
/// in that case rather than guessing a session.
///
/// `splitn(4, ':')` caps the split at the tag, so a string-typed original
/// token containing its own `:` characters (e.g. a URI-shaped token) is
/// preserved intact in the fourth part instead of being truncated.
pub(crate) fn decode(raw: &str) -> Option<(u64, RpcId)> {
    let mut parts = raw.splitn(4, ':');
    if parts.next() != Some("tmx") {
        return None;
    }
    let session = parts.next()?.parse::<u64>().ok()?;
    let tag = parts.next()?;
    let rest = parts.next()?;
    let token = match tag {
        "n" => RpcId::Number(rest.parse().ok()?),
        "s" => RpcId::String(rest.to_string()),
        _ => return None,
    };
    Some((session, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_number_token() {
        let encoded = encode(3, &RpcId::Number(42));
        assert_eq!(decode(&encoded), Some((3, RpcId::Number(42))));
    }

    #[test]
    fn roundtrips_negative_number_token() {
        let encoded = encode(1, &RpcId::Number(-7));
        assert_eq!(decode(&encoded), Some((1, RpcId::Number(-7))));
    }

    #[test]
    fn roundtrips_string_token() {
        let encoded = encode(5, &RpcId::String("7f0266a6-63a2-43f5-bf5d".to_string()));
        assert_eq!(
            decode(&encoded),
            Some((5, RpcId::String("7f0266a6-63a2-43f5-bf5d".to_string())))
        );
    }

    #[test]
    fn roundtrips_string_token_containing_colons() {
        // A pathological but spec-legal string token — the encoding must
        // not misparse the embedded colons as its own delimiters.
        let encoded = encode(2, &RpcId::String("a:b:c".to_string()));
        assert_eq!(
            decode(&encoded),
            Some((2, RpcId::String("a:b:c".to_string())))
        );
    }

    #[test]
    fn roundtrips_empty_string_token() {
        let encoded = encode(9, &RpcId::String(String::new()));
        assert_eq!(decode(&encoded), Some((9, RpcId::String(String::new()))));
    }

    #[test]
    fn distinguishes_numeric_and_string_tokens_with_the_same_text() {
        let numeric = encode(1, &RpcId::Number(5));
        let string = encode(1, &RpcId::String("5".to_string()));
        assert_ne!(numeric, string);
        assert_eq!(decode(&numeric), Some((1, RpcId::Number(5))));
        assert_eq!(decode(&string), Some((1, RpcId::String("5".to_string()))));
    }

    #[test]
    fn different_sessions_produce_different_tokens_for_identical_originals() {
        // The exact collision scenario #104 is about: two backends handing
        // out the same raw token value must not produce the same encoded
        // token.
        let a = encode(1, &RpcId::String("same".to_string()));
        let b = encode(2, &RpcId::String("same".to_string()));
        assert_ne!(a, b);
    }

    #[test]
    fn decode_rejects_non_namespaced_token() {
        assert_eq!(decode("not-a-proxy-token"), None);
        assert_eq!(decode(""), None);
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        assert_eq!(decode("tmx:1:x:5"), None);
    }

    #[test]
    fn decode_rejects_non_numeric_session() {
        assert_eq!(decode("tmx:abc:n:5"), None);
    }

    #[test]
    fn extract_reads_number_and_string_tokens() {
        let params = serde_json::json!({ "token": 5 });
        assert_eq!(extract(Some(&params)), Some(RpcId::Number(5)));

        let params = serde_json::json!({ "token": "abc" });
        assert_eq!(
            extract(Some(&params)),
            Some(RpcId::String("abc".to_string()))
        );
    }

    #[test]
    fn extract_returns_none_for_missing_or_invalid_token() {
        assert_eq!(extract(None), None);
        assert_eq!(extract(Some(&serde_json::json!({}))), None);
        assert_eq!(
            extract(Some(&serde_json::json!({ "token": 1.5 }))),
            None,
            "a float is not a valid LSP ProgressToken"
        );
    }

    #[test]
    fn namespace_rewrites_params_in_place_and_returns_original() {
        let mut params = serde_json::json!({ "token": "abc", "value": { "kind": "begin" } });
        let original = namespace(&mut params, 7).expect("valid token");
        assert_eq!(original, RpcId::String("abc".to_string()));
        assert_eq!(params["token"], "tmx:7:s:abc");
        // Other fields untouched.
        assert_eq!(params["value"]["kind"], "begin");
    }

    #[test]
    fn namespace_leaves_params_untouched_when_token_missing() {
        let mut params = serde_json::json!({ "value": { "kind": "begin" } });
        assert_eq!(namespace(&mut params, 7), None);
        assert_eq!(params, serde_json::json!({ "value": { "kind": "begin" } }));
    }
}
