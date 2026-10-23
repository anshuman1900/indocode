//! Shared types for the direct-API protocols. The engine keeps one normalized
//! conversation shape (OpenAI chat) and every protocol translates it to its
//! own wire format, so a new provider kind never touches the agent loop.
use serde_json::Value;

/// One tool invocation the model asked for.
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A completed model turn: streamed text plus any tool calls it requested.
#[derive(Clone, Debug, Default)]
pub struct Turn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
}

/// Why a stream ended before producing a turn.
#[derive(Debug)]
pub enum StreamEnd {
    /// The user pressed stop; the run ends cleanly.
    Stopped,
    Error(String),
}

/// Maps every tool call id in the conversation to its function name, so
/// protocols that identify results by name (Gemini) can match them.
pub fn tool_names_by_id(messages: &[Value]) -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    for message in messages {
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                if let Some(id) = call["id"].as_str() {
                    names.insert(id.to_owned(), call["function"]["name"].as_str().unwrap_or_default().to_owned());
                }
            }
        }
    }
    names
}

