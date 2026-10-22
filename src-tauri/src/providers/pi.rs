use super::goals::RpcReply;
use super::*;
use tokio::io::AsyncBufReadExt;

pub(super) struct Bridge(PathBuf);
impl Bridge {
    pub(super) fn configure(
        command: &mut Command,
        mode: &str,
        server: Option<&mcp_host::Server>,
    ) -> Result<Self, String> {
        let directory = std::env::temp_dir().join(format!("indocode-pi-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&directory).map_err(|e| e.to_string())?;
        let bridge = Self(directory);
        let path = bridge.0.join("bridge.mjs");
        std::fs::write(&path, include_str!("pi-extension.mjs")).map_err(|e| e.to_string())?;
        let mut config = server
            .map(mcp_host::Server::pi_config)
            .unwrap_or_else(|| json!({}));
        config["mode"] = json!(mode);
        command
            .arg("--extension")
            .arg(path)
            .env("INDOCODE_PI_BRIDGE", config.to_string());
        Ok(bridge)
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn configure(command: &mut Command, config: &RunConfig) {
    command.args(["--mode", "rpc"]);
    if !config.model.is_empty() && config.model != "default" {
        if let Some((provider, model)) = config.model.split_once('/') {
            command.args(["--provider", provider, "--model", model]);
        } else {
            command.args(["--model", &config.model]);
        }
    }
    if let Some(effort) = &config.reasoning_effort {
        command.args(["--thinking", effort]);
    }
    let omp = config.provider == "omp";
    if let Some(id) = &config.resume_id {
        // `--session` may print and read a fork prompt on the RPC pipes when the
        // ID matches another folder; `--session-id` only opens this folder's session.
        // omp has no `--session-id`; its `--resume` looks in this folder first and
        // fails instead of prompting when stdin is not a terminal.
        command.args([if omp { "--resume" } else { "--session-id" }, id]);
    }
    if omp {
        // The bridge extension is the only approval gate, as with Pi.
        command.args(["--approval-mode", "yolo"]);
    }
    if !config.instructions.is_empty() {
        command.args(["--append-system-prompt", &config.instructions]);
    }
}

pub(super) fn parse_models(data: &Value) -> Result<Vec<ModelInfo>, String> {
    let entries = data["models"]
        .as_array()
        .filter(|v| v.len() <= MODEL_CATALOG_LIMIT)
        .ok_or("Invalid Pi model catalog")?;
    let mut models = vec![ModelInfo::default_choice()];
    let mut seen = HashSet::new();
    for entry in entries {
        let provider = field(entry, "provider");
        let model = field(entry, "id");
        if !valid_model_id(provider) || !valid_model_id(model) {
            return Err("Invalid Pi model ID".into());
        }
        let id = format!("{provider}/{model}");
        if !valid_model_id(&id) {
            return Err("Invalid Pi model ID".into());
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        models.push(ModelInfo {
            name: model_text(entry["name"].as_str(), model, 160),
            description: model_text(Some(provider), "", 200),
            id,
            reasoning_efforts: if entry["reasoning"] == true {
                ["off", "minimal", "low", "medium", "high", "xhigh"]
                    .iter()
                    .map(|id| effort_info(id, None))
                    .collect()
            } else {
                vec![]
            },
            default_reasoning_effort: None,
            context_window: entry["contextWindow"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= 1_000_000_000),
        });
    }
    Ok(models)
}

pub(super) async fn models(path: &Path, provider: &str) -> Result<Vec<ModelInfo>, String> {
    let mut command = super::command(path)?;
    command.args(["--mode", "rpc", "--no-session", "--no-extensions", "--no-skills"]);
    if provider == "omp" {
        command.args(["--no-rules", "--no-lsp", "--no-title"]);
    } else {
        command.args(["--no-prompt-templates", "--no-mcp", "--no-context-files", "--offline"]);
    }
    command
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not start Pi: {e}"))?;
    let group = ProcessGroup::register(child.id());
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut stdin = child.stdin.take().ok_or("Pi stdin unavailable")?;
        write_json(
            &mut stdin,
            &json!({"type":"get_available_models","id":"indocode-models"}),
        )
        .await?;
        let mut stdout =
            tokio::io::BufReader::new(child.stdout.take().ok_or("Pi stdout unavailable")?)
                .take(1024 * 1024 + 1);
        let mut total = 0;
        loop {
            let mut line = Vec::new();
            let count = stdout
                .read_until(b'\n', &mut line)
                .await
                .map_err(|e| e.to_string())?;
            total += count;
            if total > 1024 * 1024 {
                return Err("Pi model catalog exceeded its output limit".into());
            }
            if count == 0 {
                return Err("Pi exited before returning models".into());
            }
            let value: Value =
                serde_json::from_slice(&line).map_err(|_| "Invalid Pi discovery JSON")?;
            if value["type"] == "response" && value["id"] == "indocode-models" {
                if value["success"] != true {
                    return Err(
                        "Pi could not load models. Check its login and configuration.".into(),
                    );
                }
                return parse_models(&value["data"]);
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err("Pi model discovery timed out".into()));
    drop(group);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    result
}

/// Whether a stopped run's output has reached its end.
pub(super) fn stop_settled(provider: &str, value: &Value) -> bool {
    match provider {
        // Pi stays up after `abort`; settling is the end of the stopped run.
        "pi" => value["type"] == "agent_settled",
        "omp" => value["type"] == "prompt_result" || value["type"] == "session_settled",
        _ => false,
    }
}

/// Pi run state. `agent_settled`, not `agent_end`, finishes a run: retries,
/// compaction recovery and steering can follow an `agent_end`.
#[derive(Default)]
pub(super) struct State {
    ready: bool,
    /// `get_state` can answer before the bridge reports ready; it waits here.
    held_state: Option<Value>,
    /// Latest assistant failure. A successful retry or answer clears it.
    error: Option<String>,
    settled: bool,
    /// omp's `prompt_result` outcome, held until the session settles.
    prompt_result: Option<Value>,
    steers: HashMap<String, RpcReply>,
    steer_sequence: u64,
}

const MAX_PENDING_STEERS: usize = 16;

impl Protocol {
    pub(super) fn pi(&mut self, value: Value) -> Result<Step, String> {
        let mut step = Step::default();
        match field(&value, "type") {
            "response" => self.pi_response(value, &mut step)?,
            "message_start" if value["message"]["role"] == "assistant" => {
                self.claude_message_id = self.next_text_id();
            }
            "message_update" => {
                let event = &value["assistantMessageEvent"];
                if event["type"] == "text_delta" {
                    let id = self.claude_message_id.clone();
                    self.text_delta(&id, None, field(event, "delta"), &mut step);
                } else if event["type"] == "thinking_start" {
                    step.events.push(self.event("status", "Reasoning", None));
                }
            }
            "message_end" if value["message"]["role"] == "assistant" => {
                let message = &value["message"];
                match field(message, "stopReason") {
                    // Pi may retry; agent_settled decides whether this failed the run.
                    "error" => {
                        self.pi.error = Some(short(field(message, "errorMessage")))
                    }
                    "aborted" => {}
                    _ => {
                        self.pi.error = None;
                        let text = message["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|v| v["type"] == "text")
                            .map(|v| field(v, "text"))
                            .collect::<String>();
                        let id = self.claude_message_id.clone();
                        self.text_snapshot(&id, None, &text, &mut step);
                    }
                }
            }
            "tool_execution_start" => {
                let name = field(&value, "toolName");
                let id = field(&value, "toolCallId");
                self.tool_update(
                    id,
                    Some(name),
                    Some("running"),
                    Some(preview_json(&value["args"], TOOL_INPUT_LIMIT)),
                    None,
                    &mut step,
                );
                let paths = match name {
                    "edit" | "write" => Some(
                        value["args"]["path"]
                            .as_str()
                            .map(str::to_owned)
                            .into_iter()
                            .collect(),
                    ),
                    "bash" => Some(vec![]),
                    _ => None,
                };
                self.tool_paths(id, paths, &mut step);
            }
            "tool_execution_update" | "tool_execution_end" => {
                let done = value["type"] == "tool_execution_end";
                let status = if !done {
                    "running"
                } else if value["isError"] == true {
                    "failed"
                } else {
                    "completed"
                };
                let result = if done {
                    &value["result"]
                } else {
                    &value["partialResult"]
                };
                self.tool_update(
                    field(&value, "toolCallId"),
                    Some(field(&value, "toolName")),
                    Some(status),
                    None,
                    Some(content_preview(result, TOOL_OUTPUT_LIMIT)),
                    &mut step,
                );
            }
            "auto_retry_start" => step.events.push(self.event(
                "status",
                format!(
                    "Retrying (attempt {} of {}): {}",
                    value["attempt"],
                    value["maxAttempts"],
                    field(&value, "errorMessage")
                ),
                None,
            )),
            "auto_retry_end" => {
                if value["success"] == true {
                    self.pi.error = None;
                } else if let Some(error) = value["finalError"].as_str() {
                    self.pi.error = Some(short(error));
                }
            }
            "compaction_start" => step
                .events
                .push(self.event("status", "Compacting context", None)),
            "compaction_end" => {
                if let Some(error) = value["errorMessage"].as_str() {
                    step.events.push(
                        self.event("status", format!("Context compaction failed: {error}"), None),
                    );
                } else if value["aborted"] != true {
                    step.events
                        .push(self.event("status", "Context compacted", None));
                }
            }
            "agent_settled" => self.pi_settled(value["aborted"] == true, &mut step)?,
            // omp reports each prompt's outcome once its work settles; background
            // work it started may keep the session busy until `session_settled`.
            "prompt_result" if value["id"] == "indocode-prompt" => {
                let settled = value["sessionSettled"] == true;
                self.pi.prompt_result = Some(value);
                if settled {
                    self.omp_settled(&mut step)?;
                }
            }
            "session_settled" if self.pi.prompt_result.is_some() && !self.pi.settled => {
                self.omp_settled(&mut step)?
            }
            "extension_ui_request" => self.pi_ui(value, &mut step)?,
            // Only the permission bridge is required; other extensions fail on their own.
            "extension_error" if !field(&value, "extensionPath").contains("indocode-pi") => {
                step.events.push(self.event(
                    "status",
                    format!("Pi extension error: {}", field(&value, "error")),
                    None,
                ))
            }
            "extension_error" => {
                return Err(format!(
                    "Pi extension failed: {}",
                    short(field(&value, "error"))
                ))
            }
            _ => {}
        }
        Ok(step)
    }

    fn pi_settled(&mut self, aborted: bool, step: &mut Step) -> Result<(), String> {
        self.pi.settled = true;
        if aborted {
            return Err("Pi response was aborted".into());
        }
        if let Some(error) = self.pi.error.take() {
            return Err(error);
        }
        step.write
            .push(json!({"type":"get_session_stats","id":"indocode-usage"}));
        Ok(())
    }

    fn omp_settled(&mut self, step: &mut Step) -> Result<(), String> {
        let result = self.pi.prompt_result.take().unwrap_or_default();
        if result["status"] == "error" {
            let error = result["error"]["message"].as_str().unwrap_or("omp could not finish the response");
            self.pi.error = Some(short(error));
        }
        self.pi_settled(result["status"] == "aborted", step)
    }

    fn pi_response(&mut self, value: Value, step: &mut Step) -> Result<(), String> {
        let id = field(&value, "id");
        if let Some(reply) = self.pi.steers.remove(id) {
            let _ = reply.send(if value["success"] == true {
                Ok(json!({"steered": true}))
            } else {
                Err(short(field(&value, "error")))
            });
            return Ok(());
        }
        if id == "indocode-usage" {
            // Stats are optional: a failed or unsupported request still ends the run.
            let usage = &value["data"]["contextUsage"];
            if let Some(used) = usage["tokens"].as_u64().filter(|_| value["success"] == true) {
                self.usage(used, usage["contextWindow"].as_u64(), step);
            }
            step.done = true;
            return Ok(());
        }
        if value["success"] != true {
            return Err(short(field(&value, "error")));
        }
        match id {
            "indocode-init" if !self.pi.ready => self.pi.held_state = Some(value),
            "indocode-init" => self.pi_started(&value, step)?,
            // An extension command consumed the prompt; no run started.
            "indocode-prompt" if value["data"]["disposition"] == "handled" => step.done = true,
            _ => {}
        }
        Ok(())
    }

    fn pi_started(&mut self, value: &Value, step: &mut Step) -> Result<(), String> {
        let data = &value["data"];
        self.remote_id = string(data, "sessionId");
        if uuid::Uuid::parse_str(&self.remote_id).is_err() {
            return Err("Pi did not return a valid session ID".into());
        }
        // `--session-id` creates an empty session when the saved one is missing.
        if self.config.resume_id.is_some() && data["messageCount"] == 0 {
            return Err("Pi could not find this task's saved session in this folder.".into());
        }
        step.events
            .push(self.event("session", self.remote_id.clone(), None));
        if self
            .config
            .reasoning_effort
            .as_deref()
            .is_some_and(|effort| data["thinkingLevel"].as_str() != Some(effort))
        {
            return Err("Pi did not apply the selected thinking level".into());
        }
        let model = &data["model"];
        let applied = match self.config.model.split_once('/') {
            Some((provider, id)) => model["provider"] == provider && model["id"] == id,
            None => {
                self.config.model.is_empty()
                    || self.config.model == "default"
                    || model["id"] == self.config.model.as_str()
            }
        };
        if !applied {
            return Err("Pi did not apply the selected model".into());
        }
        self.phase = 1;
        step.write.push(json!({"type":"prompt","id":"indocode-prompt","message":self.config.prompt,"images":self.image_blocks}));
        Ok(())
    }

    fn pi_ui(&mut self, value: Value, step: &mut Step) -> Result<(), String> {
        match field(&value, "method") {
            "setStatus" if value["statusKey"] == "indocode-bridge" => {
                if value["statusText"] == "ready" {
                    self.pi.ready = true;
                    if let Some(state) = self.pi.held_state.take() {
                        self.pi_started(&state, step)?;
                    }
                }
            }
            "confirm" => self.approval(
                value.clone(),
                format!("{}\n{}", field(&value, "title"), field(&value, "message")),
                step,
            )?,
            // Indocode's question form speaks Codex/MCP shapes; Pi dialogs decline for now.
            "select" | "input" | "editor" => step.write.push(
                json!({"type":"extension_ui_response","id":value["id"],"cancelled":true}),
            ),
            "notify" => step
                .events
                .push(self.event("status", field(&value, "message"), None)),
            "setStatus" if !field(&value, "statusText").is_empty() => step
                .events
                .push(self.event("status", field(&value, "statusText"), None)),
            // setTitle, setWidget and set_editor_text drive Pi's own terminal UI.
            _ => {}
        }
        Ok(())
    }

    pub(super) fn pi_steer(&mut self, text: String, reply: RpcReply) -> Step {
        let mut step = Step::default();
        if self.phase == 0 || self.pi.settled {
            let _ = reply.send(Err("The agent has no active turn to steer.".into()));
            return step;
        }
        if self.pi.steers.len() >= MAX_PENDING_STEERS {
            let _ = reply.send(Err("Too many steering messages are waiting.".into()));
            return step;
        }
        self.pi.steer_sequence += 1;
        let id = format!("indocode-steer-{}", self.pi.steer_sequence);
        step.write
            .push(json!({"type":"steer","id":id,"message":text}));
        self.pi.steers.insert(id, reply);
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Protocol {
        let mut p = Protocol::new(super::super::tests::config("pi"));
        p.receive(json!({"type":"extension_ui_request","method":"setStatus","statusKey":"indocode-bridge","statusText":"ready"})).unwrap();
        p
    }

    fn state() -> Value {
        json!({"type":"response","command":"get_state","id":"indocode-init","success":true,"data":{"sessionId":"01a15ba3-9096-76db-bd69-35168c0ecfa9","thinkingLevel":"medium"}})
    }

    #[test]
    fn bridge_must_be_ready_before_any_prompt() {
        // get_state may answer before the bridge's ready status; it waits for it.
        let mut early = Protocol::new(super::super::tests::config("pi"));
        let held = early.receive(state()).unwrap();
        assert!(held.write.is_empty() && held.events.is_empty());
        assert_eq!(early.phase, 0);
        let step = early.receive(json!({"type":"extension_ui_request","method":"setStatus","statusKey":"indocode-bridge","statusText":"ready"})).unwrap();
        assert_eq!(step.events[0].kind, "session");
        assert_eq!(step.write[0]["type"], "prompt");
        let mut p = ready();
        p.image_blocks
            .push(json!({"type":"image","mimeType":"image/png","data":"image"}));
        let step = p.receive(state()).unwrap();
        assert_eq!(step.events[0].kind, "session");
        assert_eq!(step.write[0]["type"], "prompt");
        assert_eq!(step.write[0]["images"][0]["data"], "image");
        assert_eq!(p.cancel()["type"], "abort");
    }

    #[test]
    fn selected_models_are_verified_with_or_without_a_provider() {
        let mut state = state();
        state["data"]["model"] = json!({"provider":"anthropic","id":"sonnet"});
        for (model, applied) in [("default", true), ("sonnet", true), ("anthropic/sonnet", true), ("opus", false), ("openai/sonnet", false)] {
            let mut p = ready();
            p.config.model = model.into();
            assert_eq!(p.receive(state.clone()).is_ok(), applied, "{model}");
        }
    }

    #[test]
    fn resumed_sessions_must_exist_in_this_folder() {
        let mut command = Command::new("pi");
        let mut config = super::super::tests::config("pi");
        config.resume_id = Some("01a15ba3-9096-76db-bd69-35168c0ecfa9".into());
        configure(&mut command, &config);
        let args: Vec<_> = command.as_std().get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(args.windows(2).any(|a| a == ["--session-id", "01a15ba3-9096-76db-bd69-35168c0ecfa9"]));
        let mut p = Protocol::new(config);
        p.pi.ready = true;
        let mut empty = state();
        empty["data"]["messageCount"] = json!(0);
        assert!(p.receive(empty).err().unwrap().contains("saved session"));
    }

    #[test]
    fn a_failed_turn_is_retried_before_settling() {
        let mut p = ready();
        p.receive(state()).unwrap();
        p.receive(json!({"type":"message_start","message":{"role":"assistant"}})).unwrap();
        let failed = p.receive(json!({"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"529 overloaded"}})).unwrap();
        assert!(!failed.done);
        let end = p.receive(json!({"type":"agent_end","messages":[],"willRetry":true})).unwrap();
        assert!(end.write.is_empty(), "agent_end does not finish a run");
        let retry = p.receive(json!({"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,"errorMessage":"529 overloaded"})).unwrap();
        assert!(retry.events[0].text.contains("attempt 1 of 3"));
        p.receive(json!({"type":"compaction_start","reason":"overflow"})).unwrap();
        p.receive(json!({"type":"auto_retry_end","success":true,"attempt":2})).unwrap();
        p.receive(json!({"type":"message_start","message":{"role":"assistant"}})).unwrap();
        p.receive(json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Done"}],"stopReason":"stop"}})).unwrap();
        p.receive(json!({"type":"agent_end","messages":[],"willRetry":false})).unwrap();
        let settled = p.receive(json!({"type":"agent_settled","aborted":false})).unwrap();
        assert_eq!(settled.write[0]["type"], "get_session_stats");

        let mut p = ready();
        p.receive(state()).unwrap();
        p.receive(json!({"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"529 overloaded"}})).unwrap();
        p.receive(json!({"type":"auto_retry_end","success":false,"attempt":3,"finalError":"Still overloaded"})).unwrap();
        assert_eq!(p.receive(json!({"type":"agent_settled","aborted":false})).err().unwrap(), "Still overloaded");
        let mut p = ready();
        p.receive(state()).unwrap();
        assert!(p.receive(json!({"type":"agent_settled","aborted":true})).is_err());
    }

    #[test]
    fn failed_stats_and_handled_prompts_still_finish() {
        let mut p = ready();
        p.receive(state()).unwrap();
        p.receive(json!({"type":"agent_settled","aborted":false})).unwrap();
        let end = p.receive(json!({"type":"response","command":"get_session_stats","id":"indocode-usage","success":false,"error":"Unknown command"})).unwrap();
        assert!(end.done);
        assert!(end.events.is_empty());
        let mut p = ready();
        p.receive(state()).unwrap();
        let started = p.receive(json!({"type":"response","command":"prompt","id":"indocode-prompt","success":true,"data":{"disposition":"started"}})).unwrap();
        assert!(!started.done);
        let handled = p.receive(json!({"type":"response","command":"prompt","id":"indocode-prompt","success":true,"data":{"disposition":"handled"}})).unwrap();
        assert!(handled.done);
    }

    #[test]
    fn clamped_thinking_is_reported_instead_of_silently_changed() {
        let mut p = ready();
        p.config.reasoning_effort = Some("high".into());
        assert!(p.receive(state()).err().unwrap().contains("thinking level"));
    }

    #[test]
    fn stream_snapshots_do_not_duplicate_deltas_and_tools_keep_write_paths() {
        let mut p = ready();
        p.receive(state()).unwrap();
        p.receive(json!({"type":"message_start","message":{"role":"assistant"}}))
            .unwrap();
        let step = p.receive(json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Hello"}})).unwrap();
        assert_eq!(
            step.events
                .iter()
                .filter(|e| e.kind == "text")
                .map(|e| e.text.as_str())
                .collect::<String>(),
            "Hello"
        );
        let end = p.receive(json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}],"stopReason":"stop"}})).unwrap();
        assert!(end.events.iter().all(|e| e.kind != "text"));
        let tool = p.receive(json!({"type":"tool_execution_start","toolCallId":"call","toolName":"write","args":{"path":"src/app.ts","content":"ok"}})).unwrap();
        assert_eq!(
            tool.events[0]
                .tool
                .as_ref()
                .unwrap()
                .write_paths
                .as_ref()
                .unwrap(),
            &["src/app.ts"]
        );
        let tool = p.receive(json!({"type":"tool_execution_end","toolCallId":"call","toolName":"write","result":{"content":[{"type":"text","text":"saved"}]},"isError":false})).unwrap();
        assert_eq!(tool.events[0].tool.as_ref().unwrap().status, "completed");
        assert!(p.receive(json!({"type":"agent_end","messages":[]})).unwrap().write.is_empty());
        assert_eq!(
            p.receive(json!({"type":"agent_settled","aborted":false}))
                .unwrap()
                .write[0]["type"],
            "get_session_stats"
        );
        let end = p.receive(json!({"type":"response","id":"indocode-usage","success":true,"data":{"contextUsage":{"tokens":123,"contextWindow":200000}}})).unwrap();
        assert!(end.done);
        assert_eq!(end.events[0].data.as_ref().unwrap()["usedTokens"], 123);
    }

    #[test]
    fn confirmations_and_errors_use_native_protocol() {
        let mut p = ready();
        let request = p.receive(json!({"type":"extension_ui_request","id":"dialog","method":"confirm","title":"Allow bash?","message":"pwd"})).unwrap();
        let id = request.events[0].request_id.as_ref().unwrap();
        assert_eq!(
            p.approve(id, false).unwrap(),
            json!({"type":"extension_ui_response","id":"dialog","confirmed":false})
        );
        p.receive(json!({"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"Rate limit"}})).unwrap();
        assert!(p.receive(json!({"type":"agent_settled","aborted":false})).err().unwrap().contains("Rate limit"));
        let dialog = p.receive(json!({"type":"extension_ui_request","id":"pick","method":"select","title":"Pick","options":["A"]})).unwrap();
        assert_eq!(dialog.write[0], json!({"type":"extension_ui_response","id":"pick","cancelled":true}));
    }

    #[test]
    fn model_metadata_is_provider_qualified_and_whitelisted() {
        let entries = parse_models(&json!({"models":[{"provider":"anthropic","id":"sonnet","name":"Sonnet","reasoning":true,"contextWindow":200000,"apiKey":"secret"}]})).unwrap();
        assert_eq!(entries[1].id, "anthropic/sonnet");
        assert_eq!(entries[1].context_window, Some(200000));
        assert!(!serde_json::to_string(&entries).unwrap().contains("secret"));
        assert!(
            parse_models(&json!({"models":[{"provider":"bad\nprovider","id":"model"}]})).is_err()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn installed_pi_lists_models_without_prompting() {
        let path = executable("pi").expect("Pi is installed");
        assert!(!models(&path, "pi").await.unwrap().is_empty());
    }

    fn omp_ready() -> Protocol {
        let mut p = Protocol::new(super::super::tests::config("omp"));
        p.receive(json!({"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2]})).unwrap();
        p.receive(json!({"type":"extension_ui_request","id":"s","method":"setStatus","statusKey":"indocode-bridge","statusText":"ready"})).unwrap();
        let step = p.receive(state()).unwrap();
        assert_eq!(step.write[0]["type"], "prompt");
        p
    }

    #[test]
    fn omp_resumes_with_its_own_flag_and_owns_no_approvals() {
        let mut command = Command::new("omp");
        let mut config = super::super::tests::config("omp");
        config.resume_id = Some("01a15ba3-9096-76db-bd69-35168c0ecfa9".into());
        configure(&mut command, &config);
        let args: Vec<_> = command.as_std().get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(args.windows(2).any(|a| a == ["--resume", "01a15ba3-9096-76db-bd69-35168c0ecfa9"]));
        assert!(args.windows(2).any(|a| a == ["--approval-mode", "yolo"]));
        assert!(!args.contains(&"--session-id"));
    }

    #[test]
    fn omp_finishes_on_prompt_result_once_the_session_settles() {
        // Frame shapes from omp 18.12 `modes/rpc/rpc-types.ts`.
        let mut p = omp_ready();
        p.receive(json!({"type":"response","command":"prompt","id":"indocode-prompt","success":true,"data":{"agentInvoked":true}})).unwrap();
        p.receive(json!({"type":"message_start","message":{"role":"assistant"}})).unwrap();
        p.receive(json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Hi"}})).unwrap();
        p.receive(json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Hi"}],"stopReason":"stop"}})).unwrap();
        assert!(p.receive(json!({"type":"agent_end","messages":[]})).unwrap().write.is_empty());
        assert!(p.receive(json!({"type":"session_settled"})).unwrap().write.is_empty(), "no prompt_result yet");
        let pending = p.receive(json!({"type":"prompt_result","id":"indocode-prompt","agentInvoked":true,"status":"completed","sessionSettled":false})).unwrap();
        assert!(pending.write.is_empty(), "background work still runs");
        let settled = p.receive(json!({"type":"session_settled"})).unwrap();
        assert_eq!(settled.write[0]["type"], "get_session_stats");
        assert!(p.receive(json!({"type":"session_settled"})).unwrap().write.is_empty());

        let mut p = omp_ready();
        let settled = p.receive(json!({"type":"prompt_result","id":"indocode-prompt","agentInvoked":false,"status":"completed","sessionSettled":true})).unwrap();
        assert_eq!(settled.write[0]["type"], "get_session_stats");
    }

    #[test]
    fn omp_failures_and_aborts_fail_the_run() {
        let mut p = omp_ready();
        let error = p.receive(json!({"type":"prompt_result","id":"indocode-prompt","agentInvoked":true,"status":"error","error":{"message":"429 rate limited","retryable":true},"sessionSettled":true}));
        assert!(error.err().unwrap().contains("429"));
        let mut p = omp_ready();
        assert!(p.receive(json!({"type":"prompt_result","id":"indocode-prompt","agentInvoked":true,"status":"aborted","sessionSettled":true})).is_err());
        assert!(stop_settled("omp", &json!({"type":"prompt_result"})));
        assert!(stop_settled("pi", &json!({"type":"agent_settled"})));
        assert!(!stop_settled("pi", &json!({"type":"session_settled"})));
    }

    #[tokio::test]
    #[ignore]
    async fn installed_omp_lists_models_without_prompting() {
        let path = executable("omp").expect("omp is installed");
        assert!(!models(&path, "omp").await.unwrap().is_empty());
    }
}
