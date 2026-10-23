//! Google Gemini generateContent over SSE: `contents` with `parts`, inline
//! base64 images, functionCall/functionResponse pairs and key auth. Used for
//! the `gemini` direct kind and any Gemini-compatible endpoint.
use super::types::{tool_names_by_id, StreamEnd, ToolCall, Turn};
use crate::providers::{DirectRuntime, RunInput};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

fn translate(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let names = tool_names_by_id(messages);
    let mut system: Vec<String> = Vec::new();
    let mut contents = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        match role {
            "system" => system.push(message["content"].as_str().unwrap_or_default().to_owned()),
            "assistant" => {
                let mut parts = Vec::new();
                if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
                    parts.push(json!({"text": text}));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let arguments = call["function"]["arguments"]
                        .as_str()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .unwrap_or_else(|| json!({}));
                    let name = call["function"]["name"].as_str().unwrap_or_default();
                    if name.is_empty() { continue; }
                    parts.push(json!({"functionCall": {"name": name, "args": arguments}}));
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            "tool" => {
                let name = message["tool_call_id"]
                    .as_str()
                    .and_then(|id| names.get(id).map(|name| name.as_str()))
                    .unwrap_or_default();
                if name.is_empty() { continue; }
                let payload = message["content"].as_str().unwrap_or_default();
                contents.push(json!({"role": "user", "parts": [{"functionResponse": {"name": name, "response": {"result": payload}}}]}));
            }
            _ => {
                let parts = match message["content"].as_array() {
                    Some(parts) => parts
                        .iter()
                        .map(|part| {
                            if part["type"].as_str() == Some("image_url") {
                                let url = part["image_url"]["url"].as_str().unwrap_or_default();
                                let (mime, data) = url.split_once(";base64,").unwrap_or(("image/png", url));
                                json!({"inline_data": {"mime_type": mime.trim_start_matches("data:"), "data": data}})
                            } else {
                                json!({"text": part["text"].as_str().unwrap_or_default()})
                            }
                        })
                        .collect::<Vec<_>>(),
                    None => vec![json!({"text": message["content"].as_str().unwrap_or_default()})],
                };
                contents.push(json!({"role": "user", "parts": parts}));
            }
        }
    }
    // One model turn may request several tools; every functionResponse for it
    // belongs to a single following user turn rather than one turn per result.
    let mut merged: Vec<Value> = Vec::with_capacity(contents.len());
    for content in contents {
        match merged.last_mut() {
            Some(last) if last["role"] == content["role"] && content["role"] == "user" => {
                if let (Some(parts), Some(more)) = (last["parts"].as_array_mut(), content["parts"].as_array()) {
                    parts.extend(more.iter().cloned());
                    continue;
                }
                merged.push(content);
            }
            _ => merged.push(content),
        }
    }
    ((!system.is_empty()).then(|| system.join("\n\n")), merged)
}

fn translate_tools(tools: &[Value]) -> Vec<Value> {
    let declarations: Vec<Value> = tools
        .iter()
        .filter(|tool| tool["function"]["name"].is_string())
        .map(|tool| json!({
            "name": tool["function"]["name"].as_str().unwrap_or_default(),
            "description": tool["function"]["description"].as_str().unwrap_or_default(),
            "parameters": tool["function"]["parameters"],
        }))
        .collect();
    if declarations.is_empty() {
        return Vec::new();
    }
    let wrappers = vec![json!({"functionDeclarations": declarations})];
    wrappers
}

/// Streams one assistant turn over streamGenerateContent (SSE, `alt=sse`).
pub async fn chat(
    rt: &DirectRuntime,
    model: &str,
    effort: Option<&str>,
    messages: &[Value],
    tools: &[Value],
    input: &mut UnboundedReceiver<RunInput>,
    on_delta: &(dyn Fn(&str) + Send + Sync),
) -> Result<(Turn, Vec<String>), StreamEnd> {
    let (system, contents) = translate(messages);
    let mut body = json!({"contents": contents});
    if let Some(system) = system {
        body["systemInstruction"] = json!({"parts": [{"text": system}]});
    }
    let declarations = translate_tools(tools);
    if !declarations.is_empty() {
        body["tools"] = json!(declarations);
    }
    // Gemini exposes thinking via generationConfig.thinkingConfig, not effort ids.
    if let Some(effort) = effort.filter(|effort| !effort.is_empty()) {
        let budget = match effort {
            "low" => 0,
            "medium" | "high" | "xhigh" | "max" => 4096,
            _ => 2048,
        };
        body["generationConfig"] = json!({"thinkingConfig": {"thinkingBudget": budget}});
    }

    let client = crate::cloud::http::client(std::time::Duration::from_secs(600)).map_err(StreamEnd::Error)?;
    let base = rt.base_url.trim_end_matches('/');
    let url = if rt.chat_path.is_empty() {
        format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse")
    } else {
        format!("{base}{}", rt.chat_path)
    };
    let mut request = client.post(&url);
    if let Some(key) = rt.key.as_deref().filter(|key| !key.is_empty()) {
        request = request.header("x-goog-api-key", key);
    }
    for (name, value) in &rt.extra_headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.json(&body).send().await.map_err(|error| {
        StreamEnd::Error(if error.is_connect() {
            format!("Could not reach {url}. Check the base URL and your connection.")
        } else if error.is_timeout() {
            "The model provider did not answer in time.".into()
        } else {
            format!("Model request failed: {error}")
        })
    })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = response.text().await.unwrap_or_default();
        return Err(StreamEnd::Error(crate::cloud::http::error_message("Model provider", status, &body)));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut turn = Turn::default();
    // Accumulated function-call fragments (Gemini delivers whole objects).
    let mut calls: Vec<(String, Value)> = Vec::new();
    let mut steered = Vec::new();
    loop {
        tokio::select! {
            biased;
            maybe_input = input.recv() => match maybe_input {
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
            },
            chunk = stream.next() => {
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|error| StreamEnd::Error(format!("Model stream was cut off: {error}")))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(position) = buffer.find('\n') {
                    let line: String = buffer.drain(..=position).collect();
                    let line = line.trim();
                    let Some(payload) = line.strip_prefix("data:") else { continue };
                    let payload = payload.trim();
                    if payload.is_empty() { continue; }
                    let Ok(value) = serde_json::from_str::<Value>(payload) else { continue };
                    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
                        let message = error["message"].as_str().unwrap_or("The model provider reported an error.");
                        return Err(StreamEnd::Error(message.to_owned()));
                    }
                    if let Some(usage) = value.get("usageMetadata").filter(|usage| usage.is_object()) {
                        turn.prompt_tokens = usage["promptTokenCount"].as_i64().or(turn.prompt_tokens);
                        turn.completion_tokens = usage["candidatesTokenCount"].as_i64().or(turn.completion_tokens);
                    }
                    for candidate in value["candidates"].as_array().into_iter().flatten() {
                        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
                            if let Some(text) = part["text"].as_str() {
                                if !text.is_empty() {
                                    turn.text.push_str(text);
                                    on_delta(text);
                                }
                            }
                            if let Some(call) = part["functionCall"].as_object() {
                                let name = call.get("name").and_then(|name| name.as_str()).unwrap_or_default().to_owned();
                                let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                                if !name.is_empty() { calls.push((name, args)); }
                            }
                        }
                    }
                }
            }
        }
    }
    finish(turn, calls, steered)
}

fn finish(
    mut turn: Turn,
    calls: Vec<(String, Value)>,
    steered: Vec<String>,
) -> Result<(Turn, Vec<String>), StreamEnd> {
    for (index, (name, arguments)) in calls.into_iter().enumerate() {
        turn.tool_calls.push(ToolCall { id: format!("gemini_{index}"), name, arguments });
    }
    Ok((turn, steered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_are_matched_by_function_name() {
        let (_, contents) = translate(&[
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "abc", "function": {"name": "shell", "arguments": "{\"command\":\"ls\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "abc", "content": "a.txt"}),
        ]);
        assert_eq!(contents[0]["parts"][0]["functionCall"]["name"], "shell");
        assert_eq!(contents[1]["role"], "user");
        assert_eq!(contents[1]["parts"][0]["functionResponse"]["name"], "shell");
        assert_eq!(contents[1]["parts"][0]["functionResponse"]["response"]["result"], "a.txt");
    }

    #[test]
    fn tool_results_without_a_matching_call_are_dropped() {
        let (_, contents) = translate(&[json!({"role": "tool", "tool_call_id": "gone", "content": "x"})]);
        assert!(contents.is_empty(), "an unmatched tool result must not reach Gemini");
    }

    #[test]
    fn declarations_are_wrapped_once() {
        let tools = vec![
            json!({"type": "function", "function": {"name": "shell", "description": "run", "parameters": {"type": "object"}}}),
            json!({"type": "function", "function": {"name": "read_file", "description": "read", "parameters": {"type": "object"}}}),
        ];
        let declared = translate_tools(&tools);
        assert_eq!(declared.len(), 1, "all declarations live in one tool entry");
        assert_eq!(declared[0]["functionDeclarations"].as_array().unwrap().len(), 2);
        assert_eq!(declared[0]["functionDeclarations"][0]["name"], "shell");
        assert_eq!(declared[0]["functionDeclarations"][1]["parameters"]["type"], "object");
    }

    #[test]
    fn no_tools_means_no_tools_key() {
        assert!(translate_tools(&[]).is_empty());
    }

    #[test]
    fn images_become_inline_data() {
        let (_, contents) = translate(&[
            json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/webp;base64,QUJD"}}]}),
        ]);
        assert_eq!(contents[0]["parts"][0]["inline_data"]["mime_type"], "image/webp");
        assert_eq!(contents[0]["parts"][0]["inline_data"]["data"], "QUJD");
    }

    #[test]
    fn parallel_tool_results_merge_into_one_user_turn() {
        // Regression: every functionResponse for a model turn must ride in the
        // single following user turn, not one user turn per result.
        let (_, contents) = translate(&[
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "function": {"name": "shell", "arguments": "{\"command\":\"ls\"}"}},
                {"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "a.txt"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "hi"}),
        ]);
        assert_eq!(contents.len(), 2, "the model turn and one combined user turn");
        assert_eq!(contents[1]["role"], "user");
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "both functionResponses ride in one turn");
        assert_eq!(parts[0]["functionResponse"]["name"], "shell");
        assert_eq!(parts[1]["functionResponse"]["name"], "read_file");
    }

    #[tokio::test]
    async fn streams_text_and_function_calls() {
        let sse = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"working\"}]}}],\"usageMetadata\":{\"promptTokenCount\":9}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"shell\",\"args\":{\"command\":\"ls\"}}}]}}]}\n\n",
        );
        let (base, _seen) = crate::cloud::http::mock::serve(move |_, _| (200, sse.as_bytes().to_vec()));
        let rt = DirectRuntime {
            kind: "gemini".into(),
            base_url: base,
            key: Some("k".into()),
            default_model: None,
            chat_path: String::new(),
            extra_headers: Vec::new(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunInput>();
        std::mem::forget(tx);
        let streamed = std::sync::Mutex::new(String::new());
        let (turn, steered) = chat(
            &rt,
            "gemini-2.5-pro",
            None,
            &[json!({"role": "user", "content": "list files"})],
            &[json!({"type": "function", "function": {"name": "shell", "description": "run", "parameters": {"type": "object"}}})],
            &mut rx,
            &|delta| streamed.lock().unwrap().push_str(delta),
        )
        .await
        .unwrap();
        assert_eq!(turn.text, "working");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "shell");
        assert_eq!(turn.tool_calls[0].arguments["command"], "ls");
        assert_eq!(turn.prompt_tokens, Some(9));
        assert!(steered.is_empty());
    }
}
