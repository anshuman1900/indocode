//! OpenAI-compatible chat-completions over SSE: one protocol covers OpenAI,
//! OpenRouter, Groq, Mistral, xAI, DeepSeek API, Together, LM Studio, llama.cpp
//! and any custom gateway that speaks the same shape.
use crate::providers::{DirectRuntime, RunInput};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

pub use super::types::{StreamEnd, ToolCall, Turn};

const MAX_TOOL_NAME: usize = 256;

fn auth_headers(rt: &DirectRuntime, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    if rt.kind != "ollama" {
        if let Some(key) = rt.key.as_deref().filter(|k| !k.is_empty()) {
            request = request.bearer_auth(key);
        }
    }
    request
}

/// Streams one assistant turn. Text deltas go to `on_delta` as they arrive
/// (the transcript appends each fragment). `input` is drained for Stop and
/// Steer: stop aborts the stream, steer texts are queued and returned so the
/// engine can inject them at the next tool boundary.
pub async fn chat(
    rt: &DirectRuntime,
    model: &str,
    effort: Option<&str>,
    messages: &[Value],
    tools: &[Value],
    input: &mut UnboundedReceiver<RunInput>,
    on_delta: &(dyn Fn(&str) + Send + Sync),
) -> Result<(Turn, Vec<String>), StreamEnd> {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(effort) = effort.filter(|e| !e.is_empty()) {
        body["reasoning_effort"] = json!(effort);
    }
    let client = crate::cloud::http::client(std::time::Duration::from_secs(600))
        .map_err(StreamEnd::Error)?;
    let url = format!("{}{}", rt.base_url.trim_end_matches('/'), if rt.chat_path.starts_with('/') { rt.chat_path.clone() } else { format!("/{}", rt.chat_path) });
    let mut request = auth_headers(rt, client.post(&url).json(&body));
    for (name, value) in &rt.extra_headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|e| {
            if e.is_connect() {
                StreamEnd::Error(format!("Could not reach {url}. Check the base URL and your connection."))
            } else if e.is_timeout() {
                StreamEnd::Error("The model provider did not answer in time.".into())
            } else {
                StreamEnd::Error(format!("Model request failed: {e}"))
            }
        })?;

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = response.text().await.unwrap_or_default();
        return Err(StreamEnd::Error(crate::cloud::http::error_message(
            "Model provider", status, &body,
        )));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut turn = Turn::default();
    let mut fragments: Vec<(u32, Option<String>, String, String)> = Vec::new(); // index, id, name, arguments
    let mut steered = Vec::new();
    loop {
        tokio::select! {
            biased;
            maybe_input = input.recv() => {
                match maybe_input {
                    Some(RunInput::Stop) | None => return Err(StreamEnd::Stopped),
                    Some(RunInput::Steer { text, reply }) => {
                        let _ = reply.send(Ok(json!({"queued": true})));
                        steered.push(text);
                    }
                    Some(RunInput::Approval { .. }) => {}
                    Some(RunInput::UserInput { reply, .. }) => {
                        if let Some(reply) = reply { let _ = reply.send(Ok(())); }
                    }
                    Some(RunInput::Goal { reply, .. }) => {
                        let _ = reply.send(Err("This control operation needs a CLI harness.".into()));
                    }
                    Some(RunInput::CodexRequest { reply, .. }) => {
                        let _ = reply.send(Err("This control operation needs a CLI harness.".into()));
                    }
                }
            }
            chunk = stream.next() => {
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|e| StreamEnd::Error(format!("Model stream was cut off: {e}")))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(position) = buffer.find('\n') {
                    let line: String = buffer.drain(..=position).collect();
                    let line = line.trim();
                    let Some(payload) = line.strip_prefix("data:") else { continue };
                    let payload = payload.trim();
                    if payload == "[DONE]" {
                        return finish_turn(turn, fragments, steered);
                    }
                    let Ok(value) = serde_json::from_str::<Value>(payload) else { continue };
                    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
                        let message = error["message"].as_str().unwrap_or("The model provider reported an error.");
                        return Err(StreamEnd::Error(message.to_owned()));
                    }
                    if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
                        turn.prompt_tokens = usage["prompt_tokens"].as_i64().or(turn.prompt_tokens);
                        turn.completion_tokens = usage["completion_tokens"].as_i64().or(turn.completion_tokens);
                    }
                    let Some(delta) = value["choices"][0]["delta"].as_object() else { continue };
                    if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                        if !text.is_empty() {
                            turn.text.push_str(text);
                            on_delta(text);
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                        for call in calls {
                            let index = call["index"].as_u64().unwrap_or(0) as u32;
                            let entry = match fragments.iter_mut().find(|(i, ..)| *i == index) {
                                Some(entry) => entry,
                                None => {
                                    fragments.push((index, None, String::new(), String::new()));
                                    fragments.last_mut().unwrap()
                                }
                            };
                            if let Some(id) = call["id"].as_str() { entry.1 = Some(id.to_owned()); }
                            if let Some(name) = call["function"]["name"].as_str() {
                                let mut name = name.to_owned();
                                name.truncate(MAX_TOOL_NAME);
                                entry.2.push_str(&name);
                            }
                            if let Some(arguments) = call["function"]["arguments"].as_str() {
                                entry.3.push_str(arguments);
                            }
                        }
                    }
                }
            }
        }
    }
    // Stream closed without [DONE]: keep what we have (some gateways do this).
    finish_turn(turn, fragments, steered)
}

/// Folds the accumulated tool-call fragments into the turn. Malformed argument
/// JSON fails the turn (a retry would just repeat it).
fn finish_turn(
    mut turn: Turn,
    fragments: Vec<(u32, Option<String>, String, String)>,
    steered: Vec<String>,
) -> Result<(Turn, Vec<String>), StreamEnd> {
    for (index, id, name, arguments) in fragments {
        let arguments = if arguments.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&arguments)
                .map_err(|_| StreamEnd::Error(format!("The model sent malformed tool arguments for {name}.")))?
        };
        turn.tool_calls.push(ToolCall {
            id: id.unwrap_or_else(|| format!("call_{index}")),
            name,
            arguments,
        });
    }
    Ok((turn, steered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::RunConfig;
    use std::path::PathBuf;

    fn runtime() -> DirectRuntime {
        DirectRuntime {
            kind: "openai_compatible".into(),
            base_url: "http://127.0.0.1:1".into(),
            key: Some("k".into()),
            default_model: None,
            chat_path: "/chat/completions".into(),
            extra_headers: Vec::new(),
        }
    }

    fn channel() -> UnboundedReceiver<RunInput> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(tx);
        rx
    }

    fn config() -> RunConfig {
        RunConfig {
            session_id: "s".into(),
            provider: "direct:test".into(),
            model: "m".into(),
            reasoning_effort: None,
            mode: "ask".into(),
            cwd: PathBuf::from("."),
            resume_id: None,
            prompt: String::new(),
            instructions: String::new(),
            skill: None,
            goal: None,
            codex_settings: None,
            images: Vec::new(),
            history: Vec::new(),
            direct: None,
        }
    }

    fn steer(text: &str) -> (tokio::sync::mpsc::UnboundedSender<RunInput>, tokio::sync::oneshot::Receiver<Result<Value, String>>) {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(RunInput::Steer { text: text.into(), reply: reply_tx }).unwrap();
        (tx, reply_rx)
    }

    #[tokio::test]
    async fn accumulates_split_tool_call_fragments() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"she\",\"arguments\":\"{\\\"comm\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"ll\",\"arguments\":\"and\\\":\\\"echo hi\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, seen) = crate::cloud::http::mock::serve(move |_, _| {
            (200, sse.as_bytes().to_vec())
        });
        let _ = &base;
        let rt = DirectRuntime { base_url: base.clone(), ..runtime() };
        let mut input = channel();
        let streamed = std::sync::Mutex::new(String::new());
        let (turn, steered) = chat(
            &rt,
            "m",
            None,
            &[json!({"role": "user", "content": "hi"})],
            &[],
            &mut input,
            &|delta| streamed.lock().unwrap().push_str(delta),
        )
        .await
        .unwrap();
        assert_eq!(turn.text, "Hello");
        assert_eq!(*streamed.lock().unwrap(), "Hello");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "c1");
        assert_eq!(turn.tool_calls[0].name, "shell");
        assert_eq!(turn.tool_calls[0].arguments["command"], "echo hi");
        assert_eq!(turn.prompt_tokens, Some(10));
        assert_eq!(turn.completion_tokens, Some(5));
        assert!(steered.is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stop_during_stream_ends_cleanly() {
        let (base, _) = crate::cloud::http::mock::serve(|_, _| {
            (200, "data: {\"choices\":[{\"delta\":{\"content\":\"slow\"}}]}\n\n".as_bytes().to_vec())
        });
        let rt = DirectRuntime { base_url: base, ..runtime() };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(RunInput::Stop).unwrap();
        match chat(&rt, "m", None, &[json!({"role": "user", "content": "x"})], &[], &mut rx, &|_| {}).await {
            Err(StreamEnd::Stopped) => {}
            other => panic!("expected Stopped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn steer_is_queued_and_answered_during_stream() {
        let (base, _) = crate::cloud::http::mock::serve(|_, _| {
            (200, "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
        });
        let rt = DirectRuntime { base_url: base, ..runtime() };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply_tx2, reply_rx2) = tokio::sync::oneshot::channel();
        tx.send(RunInput::Steer { text: "also add tests".into(), reply: reply_tx2 }).unwrap();
        let (_turn, steered) = chat(&rt, "m", None, &[json!({"role": "user", "content": "x"})], &[], &mut rx, &|_| {})
            .await
            .unwrap();
        assert_eq!(steered, vec!["also add tests".to_string()]);
        assert_eq!(reply_rx2.await.unwrap().unwrap()["queued"], true);
    }

    #[tokio::test]
    async fn error_bodies_surface_the_provider_message() {
        let (base, _) = crate::cloud::http::mock::serve(|_, _| {
            (429, r#"{"error":{"message":"Rate limit exceeded"}}"#.as_bytes().to_vec())
        });
        let rt = DirectRuntime { base_url: base, ..runtime() };
        let mut input = channel();
        let error = chat(&rt, "m", None, &[], &[], &mut input, &|_| {}).await.unwrap_err();
        match error {
            StreamEnd::Error(message) => assert!(message.contains("Rate limit") || message.contains("429"), "{message}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
