//! Anthropic Messages API over SSE: /v1/messages with `x-api-key` auth and
//! tool_use / tool_result blocks. Used for the `anthropic` direct kind and for
//! any Anthropic-compatible endpoint (Z.AI's GLM plan, some gateways).
use super::types::{StreamEnd, ToolCall, Turn};
use crate::providers::{DirectRuntime, RunInput};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic keeps the system prompt separate from the message list.
fn translate(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system: Vec<String> = Vec::new();
    let mut translated = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        match role {
            "system" => system.push(message["content"].as_str().unwrap_or_default().to_owned()),
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let arguments = call["function"]["arguments"]
                        .as_str()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call["id"].as_str().unwrap_or_default(),
                        "name": call["function"]["name"].as_str().unwrap_or_default(),
                        "input": arguments,
                    }));
                }
                if !blocks.is_empty() {
                    translated.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool" => translated.push(json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": message["tool_call_id"].as_str().unwrap_or_default(), "content": message["content"].as_str().unwrap_or_default()}],
            })),
            _ => {
                let blocks = match message["content"].as_array() {
                    Some(parts) => parts
                        .iter()
                        .map(|part| {
                            if part["type"].as_str() == Some("image_url") {
                                let url = part["image_url"]["url"].as_str().unwrap_or_default();
                                let (media_type, data) = url.split_once(";base64,").unwrap_or(("image/png", url));
                                json!({"type": "image", "source": {"type": "base64", "media_type": media_type.trim_start_matches("data:"), "data": data}})
                            } else {
                                json!({"type": "text", "text": part["text"].as_str().unwrap_or_default()})
                            }
                        })
                        .collect::<Vec<_>>(),
                    None => vec![json!({"type": "text", "text": message["content"].as_str().unwrap_or_default()})],
                };
                translated.push(json!({"role": "user", "content": blocks}));
            }
        }
    }
    // Anthropic rejects adjacent same-role messages ("roles must alternate"),
    // and one assistant round may ask for several tools: each result must land
    // as a block in a single user turn, not as its own user message.
    let mut merged: Vec<Value> = Vec::with_capacity(translated.len());
    for message in translated {
        match merged.last_mut() {
            Some(last) if last["role"] == message["role"] => {
                if let Some(blocks) = last["content"].as_array_mut() {
                    if let Some(more) = message["content"].as_array() {
                        blocks.extend(more.iter().cloned());
                        continue;
                    }
                }
                merged.push(message);
            }
            _ => merged.push(message),
        }
    }
    ((!system.is_empty()).then(|| system.join("\n\n")), merged)
}

fn translate_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| tool["function"]["name"].is_string())
        .map(|tool| json!({
            "name": tool["function"]["name"].as_str().unwrap_or_default(),
            "description": tool["function"]["description"].as_str().unwrap_or_default(),
            "input_schema": tool["function"]["parameters"],
        }))
        .collect()
}

/// Streams one assistant turn over the Messages API.
pub async fn chat(
    rt: &DirectRuntime,
    model: &str,
    effort: Option<&str>,
    messages: &[Value],
    tools: &[Value],
    input: &mut UnboundedReceiver<RunInput>,
    on_delta: &(dyn Fn(&str) + Send + Sync),
) -> Result<(Turn, Vec<String>), StreamEnd> {
    let (system, messages) = translate(messages);
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "max_tokens": 8192,
    });
    if let Some(system) = system {
        body["system"] = json!(system);
    }
    // Anthropic has no reasoning_effort; extended thinking replaces it.
    if let Some(effort) = effort.filter(|effort| !effort.is_empty()) {
        let budget = match effort {
            "low" => 1024,
            "medium" | "high" | "xhigh" | "max" => 4096,
            _ => 2048,
        };
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
    let tool_declarations = translate_tools(tools);
    if !tool_declarations.is_empty() {
        body["tools"] = json!(tool_declarations);
    }

    let client = crate::cloud::http::client(std::time::Duration::from_secs(600)).map_err(StreamEnd::Error)?;
    let base = rt.base_url.trim_end_matches('/');
    let url = if rt.chat_path.is_empty() { format!("{base}/v1/messages") } else { format!("{base}{}", rt.chat_path) };
    let mut request = client
        .post(&url)
        .header("x-api-key", rt.key.as_deref().unwrap_or_default())
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&body);
    for (name, value) in &rt.extra_headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|error| {
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
    // index -> (id, name, arguments)
    let mut tool_blocks: Vec<(String, String, String)> = Vec::new();
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
                    match value["type"].as_str().unwrap_or_default() {
                        "message_start" => {
                            if let Some(usage) = value["message"]["usage"].as_object() {
                                turn.prompt_tokens = usage.get("input_tokens").and_then(|value| value.as_i64()).or(turn.prompt_tokens);
                            }
                        }
                        "content_block_start" => {
                            let block = &value["content_block"];
                            if block["type"].as_str() == Some("tool_use") {
                                tool_blocks.push((
                                    block["id"].as_str().unwrap_or_default().to_owned(),
                                    block["name"].as_str().unwrap_or_default().to_owned(),
                                    String::new(),
                                ));
                            }
                        }
                        "content_block_delta" => {
                            let delta = &value["delta"];
                            if let Some(text) = delta["text"].as_str() {
                                if !text.is_empty() {
                                    turn.text.push_str(text);
                                    on_delta(text);
                                }
                            }
                            if let Some(partial) = delta["partial_json"].as_str() {
                                if let Some(block) = tool_blocks.last_mut() { block.2.push_str(partial); }
                            }
                        }
                        "message_delta" => {
                            if let Some(usage) = value["usage"].as_object() {
                                turn.completion_tokens = usage.get("output_tokens").and_then(|value| value.as_i64()).or(turn.completion_tokens);
                            }
                        }
                        "error" => {
                            let message = value["error"]["message"].as_str().unwrap_or("The model provider reported an error.");
                            return Err(StreamEnd::Error(message.to_owned()));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    finish(turn, tool_blocks, steered)
}

fn finish(
    mut turn: Turn,
    tool_blocks: Vec<(String, String, String)>,
    steered: Vec<String>,
) -> Result<(Turn, Vec<String>), StreamEnd> {
    for (index, (id, name, arguments)) in tool_blocks.into_iter().enumerate() {
        let arguments = if arguments.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&arguments).map_err(|_| {
                StreamEnd::Error(format!("The model sent malformed tool arguments for {name}."))
            })?
        };
        turn.tool_calls.push(ToolCall {
            id: (!id.is_empty()).then(|| id).unwrap_or_else(|| format!("toolu_{index}")),
            name,
            arguments,
        });
    }
    Ok((turn, steered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_history_blocks_and_results() {
        let (system, messages) = translate(&[
            json!({"role": "system", "content": "be terse"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "t1", "function": {"name": "shell", "arguments": "{\"command\":\"ls\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "a.txt"}),
        ]);
        assert_eq!(system.as_deref(), Some("be terse"));
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["input"]["command"], "ls");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn tool_declarations_use_input_schema() {
        let tools = vec![json!({"type": "function", "function": {"name": "shell", "description": "run", "parameters": {"type": "object"}}})];
        let declared = translate_tools(&tools);
        assert_eq!(declared[0]["name"], "shell");
        assert_eq!(declared[0]["input_schema"]["type"], "object");
        assert!(declared[0].get("function").is_none());
    }

    #[test]
    fn images_become_base64_blocks() {
        let (_, messages) = translate(&[
            json!({"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}]}),
        ]);
        // Input order is preserved: the text block, then the image block.
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "look");
        assert_eq!(messages[0]["content"][1]["type"], "image");
        assert_eq!(messages[0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(messages[0]["content"][1]["source"]["data"], "QUJD");
    }

    #[test]
    fn parallel_tool_results_merge_into_one_user_turn() {
        // Regression: Anthropic rejects adjacent same-role messages, so a round
        // with two tool calls must reach it as one user turn with two blocks.
        let (_, messages) = translate(&[
            json!({"role": "user", "content": "do two things"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "t1", "function": {"name": "shell", "arguments": "{\"command\":\"ls\"}"}},
                {"id": "t2", "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "a.txt"}),
            json!({"role": "tool", "tool_call_id": "t2", "content": "hi"}),
        ]);
        assert_eq!(messages.len(), 3, "one user turn and one assistant turn around the results");
        assert_eq!(messages[2]["role"], "user");
        let blocks = messages[2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "both results ride in the single user turn");
        assert_eq!(blocks[0]["tool_use_id"], "t1");
        assert_eq!(blocks[1]["tool_use_id"], "t2");
    }

    #[tokio::test]
    async fn streams_text_and_tool_use_blocks() {
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"shell\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"comm\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"and\\\":\\\"ls\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"listing\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
        );
        let (base, _seen) = crate::cloud::http::mock::serve(move |_, _| (200, sse.as_bytes().to_vec()));
        let rt = DirectRuntime {
            kind: "anthropic".into(),
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
            "claude",
            None,
            &[json!({"role": "user", "content": "list files"})],
            &[],
            &mut rx,
            &|delta| streamed.lock().unwrap().push_str(delta),
        )
        .await
        .unwrap();
        assert_eq!(turn.text, "listing");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "toolu_1");
        assert_eq!(turn.tool_calls[0].name, "shell");
        assert_eq!(turn.tool_calls[0].arguments["command"], "ls");
        assert_eq!(turn.prompt_tokens, Some(12));
        assert_eq!(turn.completion_tokens, Some(7));
        assert!(steered.is_empty());
    }
}
