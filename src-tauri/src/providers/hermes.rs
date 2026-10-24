//! Hermes Agent over its local ACP stdio server (`hermes acp`). Credentials stay
//! in Hermes (`hermes model` configures the provider); Indocode never stores keys
//! and never adds an auth flow of its own.
use super::*;

/// Indocode's approval policy maps onto Hermes' ACP session modes, which are its
/// edit-approval policies: `default` asks before edits, `accept_edits` allows
/// workspace edits, `dont_ask` allows every edit for the session.
pub(super) fn mode_id(mode: &str) -> &'static str {
    match mode {
        "auto" => "accept_edits",
        "full" => "dont_ask",
        _ => "default",
    }
}

/// Shown when Hermes' ACP server reports it has no inference provider configured.
pub(super) const NO_PROVIDER: &str = "Hermes Agent has no provider configured. Run `hermes model` in a terminal to pick one, then send again.";

pub(super) async fn models(path: &Path) -> Result<Vec<ModelInfo>, String> {
    if let Some(models) = acp_catalog::get("hermes", path) {
        return Ok(models);
    }
    let _refresh = acp_catalog::refresh_lock("hermes", path).await;
    if let Some(models) = acp_catalog::get("hermes", path) {
        return Ok(models);
    }
    let discovery = acp_catalog::begin("hermes", path);
    let mut command = super::command(path)?;
    command.arg("acp");
    // Hermes builds its whole agent (plugins, MCP registry, model metadata) while
    // opening the metadata session, so this cold start can take tens of seconds.
    // `acp_catalog` caches the result on disk afterwards.
    let (metadata, _) = acp::inspect(command, &std::env::temp_dir(), false).await?;
    acp_catalog::store(discovery, &metadata);
    Ok(acp::parse_models(&metadata))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_modes_map_onto_hermes_session_modes() {
        assert_eq!(mode_id("ask"), "default");
        assert_eq!(mode_id("auto"), "accept_edits");
        assert_eq!(mode_id("full"), "dont_ask");
        // Plan mode is enforced by Indocode's permission layer, never widened.
        assert_eq!(mode_id("plan"), "default");
    }

    /// Recorded from Hermes Agent 0.21.7 `session/new`; models use the legacy
    /// ACP shape with provider-encoded ids and the modes carry the policy names.
    #[test]
    fn session_catalog_parses_into_model_choices() {
        let metadata = json!({
            "sessionId": "hermes-session",
            "models": {
                "currentModelId": "openrouter:anthropic/claude-fable-5.1",
                "availableModels": [
                    {"modelId": "openrouter:anthropic/claude-fable-5.1", "name": "OpenRouter · anthropic/claude-fable-5.1", "description": "Provider: OpenRouter"},
                    {"modelId": "custom:free/deepseek-v4-flash-0731", "name": "custom · free/deepseek-v4-flash-0731"}
                ]
            },
            "modes": {
                "currentModeId": "default",
                "availableModes": [
                    {"id": "default", "name": "Default", "description": "Ask before edits."},
                    {"id": "accept_edits", "name": "Accept Edits"},
                    {"id": "dont_ask", "name": "Don't Ask"}
                ]
            }
        });
        let models = acp::parse_models(&metadata);
        assert_eq!(models[0].id, "default");
        assert!(models.iter().any(|model| model.id == "openrouter:anthropic/claude-fable-5.1"));
        assert!(models.iter().any(|model| model.id == "custom:free/deepseek-v4-flash-0731"));
        for model in models.iter().skip(1) {
            assert!(model.reasoning_efforts.is_empty(), "Hermes does not advertise per-model reasoning efforts");
        }
    }
}
