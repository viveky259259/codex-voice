//! Round-trips Claude thinking blocks through Codex `reasoning` items.
//!
//! Codex strips `encrypted_content` from every non-OpenAI history item except
//! `reasoning`, so the complete Anthropic thinking block (text plus signature,
//! or the opaque redacted payload) is stored there and replayed verbatim on the
//! next request. Claude rejects edited thinking blocks, so nothing here may
//! change the block contents.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use serde_json::json;

/// Marks `encrypted_content` values written by this proxy, so reasoning items
/// produced by other providers (for example after a model switch) are skipped.
const ENCRYPTED_CONTENT_PREFIX: &str = "anthropic-thinking-v1:";

/// Encodes a `thinking` block for storage in `Reasoning.encrypted_content`.
pub(crate) fn encode_thinking(thinking: &str, signature: &str) -> String {
    encode(&json!({
        "type": "thinking",
        "thinking": thinking,
        "signature": signature,
    }))
}

/// Encodes a `redacted_thinking` block for storage in `Reasoning.encrypted_content`.
pub(crate) fn encode_redacted_thinking(data: &str) -> String {
    encode(&json!({
        "type": "redacted_thinking",
        "data": data,
    }))
}

fn encode(block: &Value) -> String {
    format!(
        "{ENCRYPTED_CONTENT_PREFIX}{}",
        STANDARD.encode(block.to_string())
    )
}

/// Restores the Anthropic content block from `Reasoning.encrypted_content`.
///
/// Returns `None` for content this proxy did not write, malformed payloads,
/// and thinking blocks without a signature (Claude rejects those).
pub(crate) fn decode_block(encrypted_content: &str) -> Option<Value> {
    let encoded = encrypted_content.strip_prefix(ENCRYPTED_CONTENT_PREFIX)?;
    let bytes = STANDARD.decode(encoded).ok()?;
    let block: Value = serde_json::from_slice(&bytes).ok()?;
    match block.get("type").and_then(Value::as_str)? {
        "thinking" => {
            let thinking = block.get("thinking").and_then(Value::as_str)?;
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .filter(|signature| !signature.is_empty())?;
            Some(json!({
                "type": "thinking",
                "thinking": thinking,
                "signature": signature,
            }))
        }
        "redacted_thinking" => {
            let data = block.get("data").and_then(Value::as_str)?;
            Some(json!({
                "type": "redacted_thinking",
                "data": data,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn thinking_round_trips_verbatim() {
        let encoded = encode_thinking("Let me think.\n", "sig/+==");
        assert_eq!(
            decode_block(&encoded),
            Some(json!({
                "type": "thinking",
                "thinking": "Let me think.\n",
                "signature": "sig/+==",
            }))
        );
    }

    #[test]
    fn redacted_thinking_round_trips() {
        let encoded = encode_redacted_thinking("opaque");
        assert_eq!(
            decode_block(&encoded),
            Some(json!({"type": "redacted_thinking", "data": "opaque"}))
        );
    }

    #[test]
    fn rejects_foreign_or_unsigned_content() {
        assert_eq!(decode_block("gAAAAABopenai-ciphertext"), None);
        assert_eq!(decode_block(&encode_thinking("text", "")), None);
        assert_eq!(decode_block("anthropic-thinking-v1:!!!"), None);
    }
}
