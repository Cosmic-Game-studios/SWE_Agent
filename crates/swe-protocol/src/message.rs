//! Provider-neutral conversation model.
//!
//! Every LLM adapter translates between these types and its own wire format.
//! The model is deliberately close to the "content block" shape most providers
//! converged on: a message is a role plus an ordered list of typed blocks.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// Provider-specific payload that has to survive a round trip untouched.
///
/// Reasoning signatures, encrypted reasoning items and thought signatures are
/// only meaningful to the protocol (and usually the model) that produced them.
/// Adapters replay `raw` verbatim when `protocol` matches their own and drop
/// the payload otherwise.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderMeta {
    /// Wire protocol that produced the payload, e.g. `"anthropic"`.
    pub protocol: String,
    /// Model that produced the payload.
    pub model: String,
    /// The provider's original JSON for this block.
    pub raw: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<ProviderMeta>,
    },
    Image {
        /// IANA media type, e.g. `image/png`.
        media_type: String,
        /// Base64-encoded bytes (no data-URL prefix).
        data: String,
    },
    /// Model reasoning. `text` is the human-readable part and may be empty
    /// when the provider only returns an opaque/encrypted payload.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<ProviderMeta>,
    },
    ToolCall {
        id: String,
        name: String,
        /// Parsed arguments. Always a JSON object; `{}` when the model emitted
        /// arguments that could not be parsed (see `invalid_json`).
        input: Value,
        /// The raw argument text when it was not valid JSON. The tool is not
        /// executed in that case; the model gets an error result instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invalid_json: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<ProviderMeta>,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A block we do not interpret but must replay in position for the
    /// protocol that produced it (for example Anthropic's `fallback` marker).
    Opaque { meta: ProviderMeta },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            meta: None,
        }
    }

    pub fn tool_result(id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        ContentBlock::ToolResult {
            tool_call_id: id.into(),
            content: content.into(),
            is_error,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// All plain text in the message, joined with newlines.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text, .. } = block {
                if !out.is_empty() && !text.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = ToolCallRef<'_>> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolCall {
                id,
                name,
                input,
                invalid_json,
                ..
            } => Some(ToolCallRef {
                id,
                name,
                input,
                invalid_json: invalid_json.as_deref(),
            }),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls().next().is_some()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ToolCallRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub input: &'a Value,
    pub invalid_json: Option<&'a str>,
}

/// A tool the model may call. `input_schema` is a JSON Schema object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Token accounting, normalised across providers.
///
/// `input_tokens` is the *total* prompt size including cached tokens, so
/// `input_tokens + output_tokens` approximates the context in use after the
/// request. `cache_read_tokens` / `cache_write_tokens` are subsets of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }

    /// Context occupied after this request completed.
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its answer.
    EndTurn,
    /// The model wants tool results before continuing.
    ToolUse,
    /// Output was cut off at the token limit.
    MaxTokens,
    /// The provider or model declined (safety classifier, content filter).
    Refusal,
    /// Anything else the provider reported.
    Other(String),
}

/// How hard the model should think. Mapped to each provider's own knob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    #[default]
    High,
    /// The highest setting that is still meant for real work (`xhigh` where a
    /// provider has it, its top level otherwise).
    Xhigh,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
        }
    }
}

impl std::str::FromStr for Effort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Ok(Effort::Low),
            "medium" | "med" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" | "max" => Ok(Effort::Xhigh),
            other => Err(format!("unknown effort `{other}` (expected low|medium|high|xhigh)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn content_block_round_trips_with_stable_tags() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Reasoning {
                    text: "thinking".into(),
                    meta: Some(ProviderMeta {
                        protocol: "anthropic".into(),
                        model: "m".into(),
                        raw: json!({"type": "thinking", "signature": "sig"}),
                    }),
                },
                ContentBlock::text("hello"),
                ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "a.rs"}),
                    invalid_json: None,
                    meta: None,
                },
            ],
        };
        let encoded = serde_json::to_value(&msg).unwrap();
        assert_eq!(encoded["role"], "assistant");
        assert_eq!(encoded["content"][1], json!({"type": "text", "text": "hello"}));
        assert_eq!(encoded["content"][2]["type"], "tool_call");
        let decoded: Message = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(decoded.tool_calls().count(), 1);
        assert_eq!(decoded.text(), "hello");
    }

    #[test]
    fn usage_context_counts_total_prompt_plus_output() {
        let mut total = Usage::default();
        total.add(&Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 80,
            ..Default::default()
        });
        assert_eq!(total.context_tokens(), 120);
    }
}
