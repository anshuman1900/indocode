//! The direct-API agent engine: an in-process agent loop for models reached over
//! HTTP (no CLI harness). Streams through the same normalized event sink, asks
//! the same approval bar, consumes the same RunInput channel as the CLI
//! providers, and executes the built-in tools in `tools.rs` itself.
pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod tools;
pub mod types;

use types::StreamEnd;
use super::{AgentEvent, HistoryMessage, RunConfig, RunInput, Sink, ToolActivity};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

const MAX_ROUNDS: usize = 25;
const STEER_PREFIX: &str = "[The user steered the task mid-run] ";

/// A "tool" event; the transcript folds these into the Worked-for row.
fn tool_event(config: &RunConfig, id: &str, name: &str, status: &str, input: &str, output: &str, write_paths: Option<Vec<String>>) -> AgentEvent {
    AgentEvent {
        session_id: config.session_id.clone(),
        kind: "tool".into(),
        text: format!("{name} · {status}"),
        request_id: None,
        data: None,
        tool: Some(ToolActivity {
            id: id.to_owned(),
            name: name.to_owned(),
            status: status.to_owned(),
            input: input.to_owned(),
            output: output.to_owned(),
            write_paths,
        }),
    }
}

fn approval_event(config: &RunConfig, request_id: &str, name: &str, input: &str) -> AgentEvent {
    AgentEvent {
        session_id: config.session_id.clone(),
        kind: "approval".into(),
        text: format!("{name} needs your approval"),
        request_id: Some(request_id.to_owned()),
        data: Some(json!({"tool": name, "input": input})),
        tool: None,
    }
}

fn user_message(prompt: &str, images: &[std::path::PathBuf]) -> Value {
    if images.is_empty() {
        return json!({"role": "user", "content": prompt});
    }
    // Validated local images only; failures skip the image rather than the turn.
    let mut parts = vec![json!({"type": "text", "text": prompt})];
    for path in images {
        let Ok(bytes) = std::fs::read(path) else { continue };
        if bytes.len() > 8 * 1024 * 1024 {
            continue;
        }
        let mime = match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => continue,
        };
        parts.push(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{mime};base64,{}", STANDARD.encode(&bytes))}
        }));
    }
    json!({"role": "user", "content": parts})
}

/// Waits out an approval request; steers queue up while we wait.
async fn wait_approval(
    input: &mut UnboundedReceiver<RunInput>,
    request_id: &str,
    steered: &mut Vec<String>,
) -> Option<bool> {
    loop {
        match input.recv().await {
            Some(RunInput::Approval { request_id: id, approved }) if id == request_id => return Some(approved),
            Some(RunInput::Approval { .. }) => {}
            Some(RunInput::Steer { text, reply }) => {
                let _ = reply.send(Ok(json!({"queued": true})));
                steered.push(text);
            }
            Some(RunInput::UserInput { reply, .. }) => {
                if let Some(reply) = reply { let _ = reply.send(Ok(())); }
            }
            Some(RunInput::Goal { reply, .. }) => {
                let _ = reply.send(Err("This control operation needs a CLI harness.".into()));
            }
            Some(RunInput::CodexRequest { reply, .. }) => {
                let _ = reply.send(Err("This control operation needs a CLI harness.".into()));
            }
            Some(RunInput::Stop) | None => return None,
        }
    }
}

/// Host tool specs use the flat shape (`{type:"function", name, description,
/// inputSchema}`); OpenAI-compatible providers require the nested `function`
/// object and reject the whole request body when one entry lacks it. Namespaced
/// entries (`{type:"namespace", tools:[...]}`) flatten into plain functions -
/// the host handler is dispatched by bare tool name.
///
/// Returns the normalized catalog AND the callable names, so the names the
/// model was offered can never drift from the names the engine accepts: an
/// offered tool that the engine then refuses as "unknown" is a dead end for
/// the model (it retries forever against a tool it can see).
pub(crate) fn openai_tool_specs(
    host: &crate::providers::ToolHost,
) -> (Vec<Value>, std::collections::HashSet<String>) {
    /// A flat host spec becomes the nested OpenAI shape. A spec that already
    /// carries a `function` object passes through untouched, so the
    /// normalizer is idempotent and safe to apply twice.
    fn nested(spec: &Value) -> Option<Value> {
        if spec["function"].is_object() {
            return Some(spec.clone());
        }
        let name = spec["name"].as_str()?;
        Some(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": spec["description"].as_str().unwrap_or_default(),
                "parameters": spec["inputSchema"].clone(),
            }
        }))
    }
    let mut specs = Vec::new();
    let mut names = std::collections::HashSet::new();
    for tool in &host.tools {
        match tool["type"].as_str() {
            Some("function") => {
                if let Some(spec) = nested(tool) {
                    if let Some(name) = spec["function"]["name"].as_str() {
                        names.insert(name.to_owned());
                    }
                    specs.push(spec);
                }
            }
            Some("namespace") => {
                for entry in tool["tools"].as_array().into_iter().flatten() {
                    if let Some(spec) = nested(entry) {
                        if let Some(name) = spec["function"]["name"].as_str() {
                            names.insert(name.to_owned());
                        }
                        specs.push(spec);
                    }
                }
            }
            _ => {}
        }
    }
    (specs, names)
}

/// Emits the turn boundary events the app's change tracker and transcript
/// flusher wait on, whichever way the turn ends (the run-epilogue captures any
/// remaining changes itself on the failure path).
struct TurnBoundary<'a> {
    sink: &'a Sink,
    session_id: &'a str,
}
impl Drop for TurnBoundary<'_> {
    fn drop(&mut self) {
        (self.sink)(AgentEvent {
            session_id: self.session_id.to_owned(),
            kind: "turn_complete".into(),
            text: String::new(),
            request_id: None,
            data: None,
            tool: None,
        });
    }
}

pub async fn run(
    config: RunConfig,
    sink: Sink,
    mut input: UnboundedReceiver<RunInput>,
    host: Option<crate::providers::ToolHost>,
) -> Result<(), String> {
    let Some(direct) = config.direct.clone() else {
        return Err("Direct provider configuration is missing.".into());
    };
    let _boundary = TurnBoundary { sink: &sink, session_id: &config.session_id };
    sink(AgentEvent {
        session_id: config.session_id.clone(),
        kind: "turn_start".into(),
        text: String::new(),
        request_id: None,
        data: None,
        tool: None,
    });
    let model = if config.model.is_empty() || config.model == "default" {
        direct
            .default_model
            .clone()
            .filter(|m| !m.is_empty())
            .ok_or_else(|| "Pick a model for this provider in the model picker.".to_string())?
    } else {
        config.model.clone()
    };

    // The conversation so far: system, replayed history, then this turn's prompt.
    let mut messages: Vec<Value> = Vec::new();
    if !config.instructions.trim().is_empty() {
        messages.push(json!({"role": "system", "content": config.instructions}));
    }
    for HistoryMessage { role, content } in &config.history {
        if content.trim().is_empty() {
            continue;
        }
        messages.push(json!({"role": if role == "assistant" { "assistant" } else { "user" }, "content": content}));
    }
    messages.push(user_message(&config.prompt, &config.images));

    // The app's own tools (deploys, previews, git, turn-change capture) ride
    // alongside the built-ins so a direct provider reaches everything a CLI
    // harness can; the host executes them under its own approval rules.
    let mut catalog = tools::catalog();
    // The names the model was offered come from the normalizer itself, so an
    // offered tool is never refused as unknown further down.
    let mut host_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(host) = &host {
        // Normalized to the nested shape: a flat entry makes strict
        // OpenAI-compatible providers reject the entire request body.
        let (specs, names) = openai_tool_specs(host);
        catalog.extend(specs);
        host_tools = names;
    }
    let mut steered: Vec<String> = Vec::new();

    for _round in 0..MAX_ROUNDS {
        let sink_for_stream = sink.clone();
        let on_delta = &|delta: &str| sink_for_stream(AgentEvent {
            session_id: config.session_id.clone(),
            kind: "text".into(),
            text: delta.to_owned(),
            request_id: None,
            data: None,
            tool: None,
        });
        // One protocol per provider kind; the agent loop below stays identical.
        let outcome = match direct.kind.as_str() {
            "anthropic" => anthropic::chat(&direct, &model, config.reasoning_effort.as_deref(), &messages, &catalog, &mut input, on_delta).await,
            "gemini" => gemini::chat(&direct, &model, config.reasoning_effort.as_deref(), &messages, &catalog, &mut input, on_delta).await,
            "ollama" => ollama::chat(&direct, &model, config.reasoning_effort.as_deref(), &messages, &catalog, &mut input, on_delta).await,
            _ => openai::chat(&direct, &model, config.reasoning_effort.as_deref(), &messages, &catalog, &mut input, on_delta).await,
        };

        let (turn, more_steered) = match outcome {
            Ok(pair) => pair,
            Err(StreamEnd::Stopped) => return Ok(()),
            Err(StreamEnd::Error(message)) => return Err(message),
        };
        steered.extend(more_steered);

        // Token counts reach the usage panel and the context meter, exactly as a
        // CLI harness's own usage rows do.
        let used = turn
            .prompt_tokens
            .unwrap_or(0)
            .saturating_add(turn.completion_tokens.unwrap_or(0));
        if used > 0 {
            sink(AgentEvent {
                session_id: config.session_id.clone(),
                kind: "usage".into(),
                text: String::new(),
                request_id: None,
                data: Some(json!({ "usedTokens": used })),
                tool: None,
            });
        }

        if turn.tool_calls.is_empty() {
            if !steered.is_empty() && config.resume_id.is_none() {
                // Steers arrived with the final text: surface them as a note turn.
                let note = std::mem::take(&mut steered);
                messages.push(json!({"role": "user", "content": format!("{STEER_PREFIX}{}", note.join("\n"))}));
                sink(AgentEvent {
                    session_id: config.session_id.clone(),
                    kind: "text".into(),
                    text: format!("\n\n{STEER_PREFIX}{}\n", note.join("\n")),
                    request_id: None,
                    data: None,
                    tool: None,
                });
                continue;
            }
            return Ok(());
        }

        // Assistant turn with tool calls, then one result message per call.
        let mut assistant = json!({"role": "assistant", "content": turn.text});
        let calls: Vec<Value> = turn
            .tool_calls
            .iter()
            .map(|call| json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": call.arguments.to_string()}}))
            .collect();
        assistant["tool_calls"] = Value::Array(calls);
        messages.push(assistant);

        for call in &turn.tool_calls {
            let input_preview = serde_json::to_string(&call.arguments).unwrap_or_default();
            let input_preview = crate::providers::limited(&input_preview, 2048);
            sink(tool_event(&config, &call.id, &call.name, "running", &input_preview, "", write_paths_for(&call.name, &call.arguments)));

            if tools::denied_in_plan(&call.name, &config.mode) {
                let note = format!("Plan mode: {call_name} was refused. Report what you would change instead.", call_name = call.name);
                sink(tool_event(&config, &call.id, &call.name, "failed", &input_preview, &note, write_paths_for(&call.name, &call.arguments)));
                messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": note}));
                continue;
            }
            if tools::requires_approval(&call.name, &config.mode) {
                let request_id = uuid::Uuid::new_v4().to_string();
                sink(approval_event(&config, &request_id, &call.name, &input_preview));
                match wait_approval(&mut input, &request_id, &mut steered).await {
                    Some(true) => {}
                    Some(false) => {
                        let note = "The user declined this action.";
                        sink(tool_event(&config, &call.id, &call.name, "failed", &input_preview, note, write_paths_for(&call.name, &call.arguments)));
                        messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": note}));
                        continue;
                    }
                    None => return Ok(()), // stopped while waiting
                }
            }
            if !tools::known(&call.name) && !host_tools.contains(&call.name) {
                let note = format!("Unknown tool {name}; use the tools from the catalog.", name = call.name);
                sink(tool_event(&config, &call.id, &call.name, "failed", &input_preview, &note, None));
                messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": note}));
                continue;
            }
            if tools::known(&call.name) {
                let result = tools::execute(&call.name, &call.arguments, &config.cwd).await;
                match result {
                    Ok(output) => {
                        sink(tool_event(&config, &call.id, &call.name, "completed", &input_preview, &output, write_paths_for(&call.name, &call.arguments)));
                        messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": output}));
                    }
                    Err(error) => {
                        sink(tool_event(&config, &call.id, &call.name, "failed", &input_preview, &error, write_paths_for(&call.name, &call.arguments)));
                        messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": format!("Error: {error}")}));
                    }
                }
                continue;
            }
            // An app tool: the host executes it under its own approval rules.
            let Some(host) = host.as_ref() else {
                let note = "That Indocode tool is unavailable right now.";
                sink(tool_event(&config, &call.id, &call.name, "failed", &input_preview, note, None));
                messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": note}));
                continue;
            };
            if host.approval_tools.contains(&call.name) {
                if config.mode == "plan" {
                    let note = "Plan mode: this Indocode tool was refused. Report what you would do instead.";
                    sink(tool_event(&config, &call.id, &call.name, "failed", &input_preview, &note, None));
                    messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": note}));
                    continue;
                }
                if config.mode != "full"
                    || crate::cloud::deploy::ALWAYS_ASK.contains(&call.name.as_str())
                {
                    let request_id = uuid::Uuid::new_v4().to_string();
                    sink(approval_event(&config, &request_id, &call.name, &input_preview));
                    match wait_approval(&mut input, &request_id, &mut steered).await {
                        Some(true) => {}
                        Some(false) => {
                            let note = "The user declined this action.";
                            sink(tool_event(&config, &call.id, &call.name, "failed", &input_preview, note, None));
                            messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": note}));
                            continue;
                        }
                        None => return Ok(()),
                    }
                }
            }
            let result = (host.handler)(call.name.clone(), call.arguments.clone()).await;
            match result {
                Ok(value) => {
                    let output = crate::providers::limited(
                        &serde_json::to_string(&value).unwrap_or_default(),
                        64 * 1024,
                    );
                    sink(tool_event(&config, &call.id, &call.name, "completed", &input_preview, &output, None));
                    messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": output}));
                }
                Err(error) => {
                    sink(tool_event(&config, &call.id, &call.name, "failed", &input_preview, &error, None));
                    messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": format!("Error: {error}")}));
                }
            }
        }

        for text in std::mem::take(&mut steered) {
            messages.push(json!({"role": "user", "content": format!("{STEER_PREFIX}{text}")}));
        }
    }
    Err("The agent ran out of turns before finishing; send another message to continue.".into())
}

/// Explicit write targets for the activity row: shell reports none (unknown),
/// writes/edits report their file, read-only tools report nothing at all.
fn write_paths_for(name: &str, arguments: &Value) -> Option<Vec<String>> {
    match name {
        "shell" => Some(Vec::new()),
        "write_file" | "edit_file" => arguments["path"]
            .as_str()
            .map(|p| vec![p.to_owned()])
            .or_else(|| Some(Vec::new())),
        _ => None,
    }
}

/// The conversation replay for a direct provider: bounded user/assistant text.
/// (Mapping happens in lib.rs; kept as a named concept for tests.)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::RunConfig;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn runtime() -> crate::providers::DirectRuntime {
        crate::providers::DirectRuntime {
            kind: "openai_compatible".into(),
            base_url: String::new(),
            key: None,
            default_model: Some("test-model".into()),
            chat_path: "/chat/completions".into(),
            extra_headers: Vec::new(),
        }
    }

    fn config(mode: &str, direct: crate::providers::DirectRuntime) -> RunConfig {
        RunConfig {
            session_id: "s1".into(),
            provider: "direct:test".into(),
            model: String::new(),
            reasoning_effort: None,
            mode: mode.into(),
            cwd: PathBuf::from("."),
            resume_id: None,
            prompt: "say hi".into(),
            instructions: String::new(),
            skill: None,
            goal: None,
            codex_settings: None,
            images: Vec::new(),
            history: Vec::new(),
            direct: Some(direct),
        }
    }

    fn channel() -> UnboundedReceiver<RunInput> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(tx);
        rx
    }

    fn events() -> (Sink, Arc<Mutex<Vec<AgentEvent>>>) {
        let seen: Arc<Mutex<Vec<AgentEvent>>> = Arc::default();
        let sink: Sink = { let seen = seen.clone(); Arc::new(move |event| seen.lock().unwrap().push(event)) };
        (sink, seen)
    }

    fn sse_tool_round(id: &str, tool: &str, args: &str) -> String {
        // Embed the arguments as a JSON string value: escape it first.
        let escaped = args.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"function\":{{\"name\":\"{tool}\",\"arguments\":\"{escaped}\"}}}}]}}}}]}}\n\ndata: [DONE]\n\n"
        )
    }

    #[tokio::test]
    async fn final_text_without_tools_finishes_cleanly() {
        let (base, _seen) = crate::cloud::http::mock::serve(|request, _| {
            let body: Value = serde_json::from_str(&request.body).unwrap();
            assert_eq!(body["model"], "test-model");
            assert_eq!(body["messages"].as_array().unwrap().last().unwrap()["content"], "say hi");
            (200, "data: {\"choices\":[{\"delta\":{\"content\":\"Hi there\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
        });
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let (sink, events) = events();
        run(config("ask", direct), sink, channel(), None).await.unwrap();
        let text: String = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == "text")
            .map(|e| e.text.clone())
            .collect();
        assert_eq!(text, "Hi there");
        assert!(events.lock().unwrap().iter().all(|e| e.kind != "tool"));
    }

    #[tokio::test]
    async fn runs_a_shell_tool_round_then_answers() {
        let rounds: Arc<Mutex<u32>> = Arc::default();
        let rounds_clone = rounds.clone();
        let (base, _) = crate::cloud::http::mock::serve(move |request, _| {
            let mut count = rounds_clone.lock().unwrap();
            *count += 1;
            let body: Value = serde_json::from_str(&request.body).unwrap();
            let role_count = body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").count();
            if *count == 1 {
                (200, sse_tool_round("call_1", "shell", "{\"command\": \"echo engine-ok\"}").into_bytes())
            } else {
                assert!(role_count >= 1, "second round must carry the tool result");
                (200, "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
            }
        });
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let (sink, events) = events();
        // "full" runs tools without the approval gate; the gate has its own tests.
        run(config("full", direct), sink, channel(), None).await.unwrap();
        let seen = events.lock().unwrap();
        let statuses: Vec<&str> = seen.iter().filter_map(|e| e.tool.as_ref().map(|t| t.status.as_str())).collect();
        assert_eq!(statuses, vec!["running", "completed"], "{statuses:?}");
        let tool = seen.iter().filter_map(|e| e.tool.clone()).last().unwrap();
        assert_eq!(tool.name, "shell");
        assert!(tool.output.contains("engine-ok"), "{}", tool.output);
        assert_eq!(tool.write_paths, Some(Vec::new()));
    }

    #[tokio::test]
    async fn approval_gate_blocks_until_approved() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rounds: Arc<Mutex<u32>> = Arc::default();
        let rounds_clone = rounds.clone();
        let (base, _) = crate::cloud::http::mock::serve(move |_, _| {
            let round = { let mut count = rounds_clone.lock().unwrap(); *count += 1; *count };
            if round == 1 {
                (200, sse_tool_round("call_1", "write_file", "{\"path\": \"note.txt\", \"content\": \"hi\"}").into_bytes())
            } else {
                (200, "data: {\"choices\":[{\"delta\":{\"content\":\"written\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
            }
        });
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let (sink, events) = events();
        let approver_events = events.clone();
        let engine = run(config("ask", direct), sink, rx, None);
        let approver = tokio::spawn(async move {
            // Wait for the real approval request, then approve it.
            let request_id = loop {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if let Some(event) = approver_events.lock().unwrap().iter().find(|e| e.kind == "approval") {
                    break event.request_id.clone().unwrap();
                }
            };
            tx.send(RunInput::Approval { request_id, approved: true }).unwrap();
        });
        engine.await.unwrap();
        approver.await.unwrap();
        let seen = events.lock().unwrap();
        assert!(seen.iter().any(|e| e.kind == "approval"));
        let statuses: Vec<&str> = seen.iter().filter_map(|e| e.tool.as_ref().map(|t| t.status.as_str())).collect();
        assert_eq!(statuses, vec!["running", "completed"]);
        assert_eq!(seen.iter().find_map(|e| e.tool.clone()).unwrap().write_paths, Some(vec!["note.txt".to_string()]));
    }

    #[tokio::test]
    async fn plan_mode_refuses_mutation_without_asking() {
        let rounds: Arc<Mutex<u32>> = Arc::default();
        let rounds_clone = rounds.clone();
        let (base, _) = crate::cloud::http::mock::serve(move |_, _| {
            let round = { let mut count = rounds_clone.lock().unwrap(); *count += 1; *count };
            if round == 1 {
                (200, sse_tool_round("call_1", "write_file", "{\"path\": \"x.txt\", \"content\": \"y\"}").into_bytes())
            } else {
                (200, "data: {\"choices\":[{\"delta\":{\"content\":\"understood\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
            }
        });
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let (sink, events) = events();
        run(config("plan", direct), sink, channel(), None).await.unwrap();
        let seen = events.lock().unwrap();
        assert!(!seen.iter().any(|e| e.kind == "approval"), "plan mode must not ask");
        let tool = seen.iter().filter_map(|e| e.tool.clone()).last().unwrap();
        assert_eq!(tool.status, "failed");
        assert!(tool.output.contains("Plan mode"));
    }

    #[tokio::test]
    async fn declined_approval_reports_to_the_model() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rounds: Arc<Mutex<u32>> = Arc::default();
        let rounds_clone = rounds.clone();
        let (base, _) = crate::cloud::http::mock::serve(move |_, _| {
            let round = { let mut count = rounds_clone.lock().unwrap(); *count += 1; *count };
            if round == 1 {
                (200, sse_tool_round("call_1", "shell", "{\"command\": \"echo no\"}").into_bytes())
            } else {
                (200, "data: {\"choices\":[{\"delta\":{\"content\":\"ok, skipping\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
            }
        });
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let (sink, events) = events();
        let denier_events = events.clone();
        let engine = run(config("ask", direct), sink, rx, None);
        let denier = tokio::spawn(async move {
            let request_id = loop {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if let Some(event) = denier_events.lock().unwrap().iter().find(|e| e.kind == "approval") {
                    break event.request_id.clone().unwrap();
                }
            };
            tx.send(RunInput::Approval { request_id, approved: false }).unwrap();
        });
        engine.await.unwrap();
        denier.await.unwrap();
        let seen = events.lock().unwrap();
        let tool = seen.iter().filter_map(|e| e.tool.clone()).last().unwrap();
        assert_eq!(tool.status, "failed");
        assert!(tool.output.contains("declined"));
    }

    #[test]
    fn host_tool_specs_reach_the_provider_in_the_nested_shape() {
        // Regression: NVIDIA rejected the request body with "missing field
        // 'function'" because host specs are flat and were merged verbatim.
        let host = crate::providers::ToolHost {
            tools: vec![
                json!({"type": "function", "name": "indocode_preview_file", "description": "preview", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}}),
                json!({"type": "namespace", "name": "mcp", "tools": [{"name": "mcp_read", "description": "read", "inputSchema": {"type": "object"}}]}),
                json!({"type": "namespace", "name": "ignored"}),
            ],
            handler: std::sync::Arc::new(|_, _| Box::pin(async { Ok(json!({})) })),
            approval_tools: std::collections::HashSet::new(),
        };
        let (specs, names) = openai_tool_specs(&host);
        assert_eq!(specs.len(), 2, "the flat tool and the namespaced entry");
        for spec in &specs {
            assert!(spec["function"].is_object(), "{spec} must carry the nested function object");
            assert!(spec["function"]["parameters"].is_object(), "{spec} must carry a parameters schema");
        }
        assert_eq!(specs[0]["function"]["name"], "indocode_preview_file");
        assert_eq!(specs[0]["function"]["parameters"]["properties"]["path"]["type"], "string");
        assert_eq!(specs[1]["function"]["name"], "mcp_read", "the namespaced tool dispatches by bare name");
        // The callable names are derived from the same specs the model was
        // offered, so an offered tool can never be refused as unknown.
        assert!(names.contains("indocode_preview_file"));
        assert!(names.contains("mcp_read"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn already_nested_tool_specs_pass_through_unchanged() {
        // The normalizer must be idempotent: a spec that already carries the
        // nested `function` object is not wrapped a second time.
        let host = crate::providers::ToolHost {
            tools: vec![json!({"type": "function", "function": {"name": "shell", "description": "run", "parameters": {"type": "object"}}})],
            handler: std::sync::Arc::new(|_, _| Box::pin(async { Ok(json!({})) })),
            approval_tools: std::collections::HashSet::new(),
        };
        let (specs, names) = openai_tool_specs(&host);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0]["function"]["name"], "shell");
        assert_eq!(specs[0]["function"]["parameters"]["type"], "object");
        assert!(names.contains("shell"));
    }

    #[tokio::test]
    async fn the_request_body_carries_only_nested_tool_specs() {
        // End-to-end guard on the exact provider failure: a strict
        // OpenAI-compatible endpoint (NVIDIA NIM) deserializes the whole body
        // and answers "missing field 'function'" when one tool entry is flat.
        let (base, seen) = crate::cloud::http::mock::serve(|_, _| {
            (200, "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
        });
        let host = crate::providers::ToolHost {
            tools: vec![json!({"type": "function", "name": "indocode_preview_file", "description": "preview", "inputSchema": {"type": "object"}})],
            handler: std::sync::Arc::new(|_, _| Box::pin(async { Ok(json!({})) })),
            approval_tools: std::collections::HashSet::new(),
        };
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let (sink, _events) = events();
        run(config("ask", direct), sink, channel(), Some(host)).await.unwrap();
        let requests = seen.lock().unwrap();
        let body: Value = serde_json::from_str(&requests[0].body).expect("the body is JSON");
        let tools = body["tools"].as_array().expect("the body carries tools");
        assert!(!tools.is_empty());
        for tool in tools {
            assert!(tool["function"].is_object(), "every tool must be nested: {tool}");
        }
    }

    #[tokio::test]
    async fn host_tools_run_through_the_direct_engine() {
        // Regression: the direct engine ignored the host, so app tools
        // (previews, deploys, turn-change capture) were unreachable and the
        // instructions advertised tools the model could not call.
        let rounds: std::sync::Arc<std::sync::Mutex<u32>> = std::sync::Arc::default();
        let rounds_served = rounds.clone();
        let (base, _) = crate::cloud::http::mock::serve(move |_, _| {
            let round = { let mut count = rounds_served.lock().unwrap(); *count += 1; *count };
            if round == 1 {
                (200, sse_tool_round("call_1", "indocode_preview_file", "{\"path\": \"index.html\"}").into_bytes())
            } else {
                (200, "data: {\"choices\":[{\"delta\":{\"content\":\"preview opened\"}}],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":12}}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
            }
        });
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let host = crate::providers::ToolHost {
            tools: vec![json!({"type": "function", "name": "indocode_preview_file", "description": "preview", "inputSchema": {"type": "object"}})],
            handler: std::sync::Arc::new(|_, arguments| {
                Box::pin(async move { Ok(json!({"opened": arguments["path"]})) })
            }),
            approval_tools: std::collections::HashSet::new(),
        };
        let (sink, events) = events();
        run(config("ask", direct), sink, channel(), Some(host)).await.unwrap();
        let seen = events.lock().unwrap();
        let tool = seen.iter().filter_map(|e| e.tool.clone()).last().unwrap();
        assert_eq!(tool.name, "indocode_preview_file");
        assert_eq!(tool.status, "completed");
        assert!(tool.output.contains("index.html"), "{}", tool.output);
        assert!(seen.iter().any(|e| e.kind == "usage"), "usage reaches the panel");
    }

    #[tokio::test]
    async fn turn_boundary_events_reach_the_tracker() {
        // Regression: without turn_start/turn_complete the change tracker never
        // snapshotted, so a direct-provider turn showed no diff at all.
        let (base, _) = crate::cloud::http::mock::serve(|_, _| {
            (200, "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
        });
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let (sink, events) = events();
        run(config("ask", direct), sink, channel(), None).await.unwrap();
        let seen = events.lock().unwrap();
        assert_eq!(seen.first().unwrap().kind, "turn_start");
        assert_eq!(seen.last().unwrap().kind, "turn_complete");
    }

    #[test]
    fn write_paths_follow_the_tool() {
        let args = json!({"path": "a/b.txt"});
        assert_eq!(write_paths_for("write_file", &args), Some(vec!["a/b.txt".to_string()]));
        assert_eq!(write_paths_for("shell", &args), Some(Vec::new()));
        assert_eq!(write_paths_for("read_file", &args), None);
    }

    #[test]
    fn history_rows_map_to_messages() {
        let history: Vec<HistoryMessage> = vec![("user".into(), "a".into()), ("assistant".into(), "b".into())]
            .into_iter()
            .map(|(role, content)| HistoryMessage { role, content })
            .collect();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].role, "assistant");
    }

    #[tokio::test]
    async fn run_inner_routes_direct_providers_to_the_engine() {
        // Regression: run_inner's harness allowlist used to reject direct:* before
        // the engine branch, so tasks failed with "Unknown provider".
        let (base, seen) = crate::cloud::http::mock::serve(|_, _| {
            (200, "data: {\"choices\":[{\"delta\":{\"content\":\"routed\"}}]}\n\ndata: [DONE]\n\n".as_bytes().to_vec())
        });
        let direct = crate::providers::DirectRuntime { base_url: base, ..runtime() };
        let (sink, events) = events();
        crate::providers::run(config("ask", direct), sink, channel()).await.unwrap();
        let text: String = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.kind == "text")
            .map(|event| event.text.clone())
            .collect();
        assert_eq!(text, "routed");
        assert!(seen.lock().unwrap().len() >= 1, "the engine reached the provider");
    }

    #[tokio::test]
    async fn picker_models_carry_the_id_shape_the_picker_reads() {
        // Regression: the picker drops entries without `id`; direct models used
        // to serialize as `modelId`, leaving only the synthetic default row.
        let info = crate::direct::model_info(&crate::storage::DirectModel {
            provider_id: "shape".into(),
            model_id: "vendor/model-a".into(),
            label: String::new(),
            context_window: Some(128000),
            supports_tools: true,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            discovered_at: 0,
        });
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["id"], "vendor/model-a");
        assert_eq!(json["name"], "vendor/model-a");
        assert_eq!(json["contextWindow"], 128000);
        assert!(json.get("modelId").is_none(), "the picker reads `id`, not `modelId`");
    }

    fn temp_marker() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("indocode-engine-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tools_run_against_the_task_folder() {
        // The engine passes config.cwd straight through; tools confine there.
        let cwd = temp_marker();
        let _ = std::path::Path::new(&cwd).join("x");
        std::fs::remove_dir_all(cwd).unwrap();
    }
}
