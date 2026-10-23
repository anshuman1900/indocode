//! Ollama's local chat API (/api/chat): no key, newline-delimited JSON
//! instead of SSE, and an options block instead of reasoning_effort.
use super::types::{StreamEnd, ToolCall, Turn};
use crate::providers::{DirectRuntime, RunInput};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

fn translate(messages: &[Value]) -> Vec<Value> {
    let mut translated = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        match role {
            "system" => translated.push(json!({"role": "system", "content": message["content"].as_str().unwrap_or_default()})),
            "assistant" => {
                let mut entry = json!({
                    "role": "assistant",
                    "content": message["content"].as_str().unwrap_or_default(),
                });
                let calls: Vec<Value> = message["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|call| {
                        let arguments = call["function"]["arguments"]
                            .as_str()
                            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                            .unwrap_or_else(|| json!({}));
                        json!({"function": {"name": call["function"]["name"].as_str().unwrap_or_default(), "arguments": arguments}})
                    })
                    .collect();
                if !calls.is_empty() {
                    entry["tool_calls"] = Value::Array(calls);
                }
                translated.push(entry);
            }
            "tool" => translated.push(json!({
                "role": "tool",
                "content": message["content"].as_str().unwrap_or_default(),
            })),
            _ => {
                let content = match message["content"].as_array() {
                    Some(parts) => {
                        let mut pieces = Vec::new();
                        for part in parts {
                            if part["type"].as_str() == Some("image_url") {
                                let url = part["image_url"]["url"].as_str().unwrap_or_default();
                                let (_, data) = url.split_once(";base64,").unwrap_or(("", url));
                                pieces.push(data.to_owned());
                            } else {
                                pieces.push(part["text"].as_str().unwrap_or_default().to_owned());
                            }
                        }
                        if pieces.is_empty() { message["content"].as_str().unwrap_or_default().to_owned() } else { pieces.join("\n") }
                    }
                    None => message["content"].as_str().unwrap_or_default().to_owned(),
                };
                translated.push(json!({"role": "user", "content": content}));
            }
        }
    }
    translated
}

/// Ollama accepts the OpenAI function-tool shape directly.
fn translate_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| tool["function"]["name"].is_string())
        .map(|tool| tool["function"].clone())
        .collect()
}

/// Streams one assistant turn. Each line is a complete JSON object; the turn
/// ends on the line carrying `"done": true`.
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
        "messages": translate(messages),
        "stream": true,
    });
    let tool_declarations = translate_tools(tools);
    if !tool_declarations.is_empty() {
        body["tools"] = json!(tool_declarations);
    }
    // Ollama exposes thinking through num_ctx/temperature options, not effort ids.
    if let Some(effort) = effort.filter(|effort| !effort.is_empty()) {
        let temperature = match effort {
            "low" => 0.3,
            "medium" | "high" | "xhigh" | "max" => 0.6,
            _ => 0.8,
        };
        body["options"] = json!({"temperature": temperature});
    }

    let client = crate::cloud::http::client(std::time::Duration::from_secs(600)).map_err(StreamEnd::Error)?;
    let base = rt.base_url.trim_end_matches('/');
    let url = if rt.chat_path.is_empty() { format!("{base}/api/chat") } else { format!("{base}{}", rt.chat_path) };
    let mut request = client.post(&url).json(&body);
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
    let mut steered = Vec::new();
    let mut done = false;
    let mut tool_index = 0usize;
    while !done {
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
                    if line.is_empty() { continue; }
                    let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
                    if let Some(message) = value["message"].as_object() {
                        if let Some(text) = message.get("content").and_then(|content| content.as_str()) {
                            if !text.is_empty() {
                                turn.text.push_str(text);
                                on_delta(text);
                            }
                        }
                        for call in message.get("tool_calls").and_then(|calls| calls.as_array()).into_iter().flatten() {
                            let name = call["function"]["name"].as_str().unwrap_or_default().to_owned();
                            if !name.is_empty() {
                                let arguments = call["function"]["arguments"]
                                    .as_str()
                                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                                    .unwrap_or_else(|| call["function"]["arguments"].clone());
                                turn.tool_calls.push(ToolCall { id: format!("ollama_{tool_index}"), name, arguments });
                                tool_index += 1;
                            }
                        }
                    }
                    turn.prompt_tokens = value["prompt_eval_count"].as_i64().or(turn.prompt_tokens);
                    turn.completion_tokens = value["eval_count"].as_i64().or(turn.completion_tokens);
                    if value["done"].as_bool() == Some(true) {
                        done = true;
                        break;
                    }
                }
            }
        }
    }
    Ok((turn, steered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_and_calls_use_the_ollama_shape() {
        let messages = translate(&[
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "list"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "t1", "function": {"name": "shell", "arguments": "{\"command\":\"ls\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "a.txt"}),
        ]);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["arguments"]["command"], "ls");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["content"], "a.txt");
    }

    #[test]
    fn tool_definitions_are_bare_functions() {
        let declared = translate_tools(&[json!({"type": "function", "function": {"name": "shell", "description": "run", "parameters": {"type": "object"}}})]);
        assert_eq!(declared[0]["name"], "shell");
        assert!(declared[0].get("function").is_none());
    }

    #[test]
    fn image_parts_are_joined_into_text() {
        let messages = translate(&[
            json!({"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}]}),
        ]);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "look\nQUJD");
    }

    #[tokio::test]
    async fn streams_ndjson_until_done() {
        let body = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"}}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"prompt_eval_count\":3}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}}]},\"done\":true,\"eval_count\":11}\n",
        );
        let (base, _seen) = crate::cloud::http::mock::serve(move |_, _| (200, body.as_bytes().to_vec()));
        let rt = DirectRuntime {
            kind: "ollama".into(),
            base_url: base,
            key: None,
            default_model: None,
            chat_path: String::new(),
            extra_headers: Vec::new(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunInput>();
        std::mem::forget(tx);
        let streamed = std::sync::Mutex::new(String::new());
        let (turn, steered) = chat(
            &rt,
            "qwen3",
            None,
            &[json!({"role": "user", "content": "hi"})],
            &[],
            &mut rx,
            &|delta| streamed.lock().unwrap().push_str(delta),
        )
        .await
        .unwrap();
        assert_eq!(turn.text, "Hello");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "shell");
        assert_eq!(turn.tool_calls[0].arguments["command"], "ls");
        assert_eq!(turn.prompt_tokens, Some(3));
        assert_eq!(turn.completion_tokens, Some(11));
        assert!(steered.is_empty());
    }
}
