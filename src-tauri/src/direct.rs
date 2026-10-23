//! Direct-API model providers: configuration, keys, model discovery and connection
//! tests. The provider configuration (non-secret) lives in SQLite; API keys live only
//! in the OS secure store under one service. Turn execution is in `providers/direct/`.
use crate::cloud::http;
use crate::secret_store;
use crate::storage::{DirectModel, DirectModelRow, DirectProvider, DirectProviderDraft};
use serde::Serialize;
use std::time::{Duration, Instant};
use tauri::State;

pub(crate) const SECRET_SERVICE: &str = "app.indocode.direct";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DIRECT_MODELS_LIMIT: usize = 500;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectProviderStatus {
    #[serde(flatten)]
    pub provider: DirectProvider,
    pub has_key: bool,
    pub key_hint: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectTestResult {
    pub ok: bool,
    pub latency_ms: u128,
    pub error: Option<String>,
}

fn secret_account(id: &str) -> String {
    format!("provider:{id}")
}

fn with_key(provider: DirectProvider) -> DirectProviderStatus {
    let stored = secret_store::get(SECRET_SERVICE, &secret_account(&provider.id));
    DirectProviderStatus {
        key_hint: stored.as_ref().map(|key| {
            let tail: String = key.chars().rev().take(4).collect();
            format!("••••{tail}")
        }),
        has_key: stored.is_some(),
        provider,
    }
}

fn require_provider(state: &crate::AppState, id: &str) -> Result<DirectProvider, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    store.direct_provider(id)
}

fn trim_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_owned()
}

/// Resolves a stored provider into the runtime handle the engine turns use
/// (endpoint, key from the secure store, default model).
pub(crate) fn runtime_for(provider: &DirectProvider) -> crate::providers::DirectRuntime {
    crate::providers::DirectRuntime {
        kind: provider.kind.clone(),
        base_url: api_base(provider),
        key: secret_store::get(SECRET_SERVICE, &secret_account(&provider.id)),
        default_model: provider.default_model.clone(),
    }
}

/// Resolved API root: explicit base_url, else the kind's default endpoint.
pub(crate) fn api_base(provider: &DirectProvider) -> String {
    let base = provider
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (provider.kind.as_str(), base) {
        (_, Some(base)) => trim_base(base),
        ("anthropic", _) => "https://api.anthropic.com".into(),
        ("gemini", _) => "https://generativelanguage.googleapis.com".into(),
        ("ollama", _) => "http://127.0.0.1:11434".into(),
        _ => "https://api.openai.com/v1".into(),
    }
}

/// GET request that only proves the endpoint + credentials work (never spends tokens).
fn catalog_request(provider: &DirectProvider, key: Option<&str>) -> reqwest::RequestBuilder {
    let client = http::client(HTTP_TIMEOUT).expect("network client");
    let base = api_base(provider);
    match provider.kind.as_str() {
        "anthropic" => client
            .get(format!("{base}/v1/models"))
            .header("x-api-key", key.unwrap_or_default())
            .header("anthropic-version", "2023-06-01"),
        "gemini" => client.get(format!("{base}/v1beta/models?key={}", key.unwrap_or_default())),
        "ollama" => client.get(format!("{base}/api/tags")),
        _ => client
            .get(format!("{base}/models"))
            .bearer_auth(key.unwrap_or_default()),
    }
}

/// Parses one provider's model catalog into rows. OpenRouter-style pricing
/// (USD-per-token strings) converts to integer **cents per million tokens**
/// (×1e8), so GPT-4o-mini's $0.15/Mtok input stores as 15.
pub(crate) fn parse_models(provider: &DirectProvider, value: &serde_json::Value) -> Vec<DirectModelRow> {
    let items = value["data"]
        .as_array()
        .or(value["models"].as_array())
        .filter(|items| !items.is_empty())
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for item in items {
        // OpenAI: {id}; Anthropic: {id, display_name}; Gemini: {name: "models/x",
        // displayName, inputTokenLimit}; OpenRouter: {id, name, context_length,
        // pricing:{prompt, completion}}; Ollama: {name, details}.
        let id = item["id"]
            .as_str()
            .or(item["name"].as_str())
            .unwrap_or_default()
            .trim_start_matches("models/")
            .to_owned();
        if id.is_empty() {
            continue;
        }
        let label = item["display_name"]
            .as_str()
            .or(item["displayName"].as_str())
            .or(item["name"].as_str().filter(|_| item["id"].is_string()))
            .unwrap_or(&id)
            .to_owned();
        let context = item["context_length"]
            .as_i64()
            .or(item["inputTokenLimit"].as_i64())
            .or(item["context_window"].as_i64());
        let tools = item["supportedGenerationMethods"]
            .as_array()
            .map(|methods| methods.iter().filter_map(|m| m.as_str()).any(|m| m.contains("generateContent")))
            .unwrap_or(true);
        // USD/token (string or number) → cents per million tokens.
        let cents = |per_token: &serde_json::Value| -> Option<i64> {
            let rate: f64 = per_token.as_str().and_then(|s| s.parse().ok()).or(per_token.as_f64())?;
            Some((rate * 1.0e8).round() as i64)
        };
        let (input, output) = (cents(&item["pricing"]["prompt"]).or_else(|| cents(&item["pricing"]["input"])), cents(&item["pricing"]["completion"]).or_else(|| cents(&item["pricing"]["output"])));
        rows.push(DirectModelRow {
            model_id: id,
            label,
            context_window: context,
            supports_tools: tools,
            input_price_per_mtok: input,
            output_price_per_mtok: output,
        });
        if rows.len() >= DIRECT_MODELS_LIMIT {
            break;
        }
    }
    let _ = provider;
    rows
}

#[tauri::command]
pub fn direct_providers_list(state: State<'_, crate::AppState>) -> Result<Vec<DirectProviderStatus>, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    Ok(store
        .direct_providers()?
        .into_iter()
        .map(|provider| with_key(provider))
        .collect())
}

#[tauri::command]
pub async fn direct_provider_save(
    state: State<'_, crate::AppState>,
    provider: DirectProviderDraft,
    key: Option<String>,
) -> Result<DirectProviderStatus, String> {
    let saved = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        store.direct_provider_save(provider)?
    };
    match key {
        Some(key) if key.trim().is_empty() => {
            secret_store::remove(SECRET_SERVICE, &secret_account(&saved.id));
        }
        Some(key) => secret_store::set(SECRET_SERVICE, &secret_account(&saved.id), &key)?,
        None => {}
    }
    Ok(with_key(saved))
}

#[tauri::command]
pub fn direct_provider_delete(state: State<'_, crate::AppState>, id: String) -> Result<(), String> {
    {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        store.direct_provider_delete(&id)?;
    }
    secret_store::remove(SECRET_SERVICE, &secret_account(&id));
    Ok(())
}

#[tauri::command]
pub async fn direct_provider_set_key(
    state: State<'_, crate::AppState>,
    id: String,
    key: String,
) -> Result<DirectProviderStatus, String> {
    let saved = require_provider(&state, &id)?;
    if key.trim().is_empty() {
        secret_store::remove(SECRET_SERVICE, &secret_account(&id));
    } else {
        secret_store::set(SECRET_SERVICE, &secret_account(&id), &key)?;
    }
    Ok(with_key(saved))
}

#[tauri::command]
pub async fn direct_provider_test(state: State<'_, crate::AppState>, id: String) -> Result<DirectTestResult, String> {
    let provider = require_provider(&state, &id)?;
    let key = secret_store::get(SECRET_SERVICE, &secret_account(&provider.id));
    let started = Instant::now();
    let outcome = http::send("Model provider", catalog_request(&provider, key.as_deref())).await;
    Ok(DirectTestResult {
        ok: outcome.is_ok(),
        latency_ms: started.elapsed().as_millis(),
        error: outcome.err(),
    })
}

/// Re-runs model discovery for one provider and replaces its cached catalog.
#[tauri::command]
pub async fn direct_models_refresh(
    state: State<'_, crate::AppState>,
    id: String,
) -> Result<Vec<DirectModel>, String> {
    let provider = require_provider(&state, &id)?;
    let key = secret_store::get(SECRET_SERVICE, &secret_account(&provider.id));
    let value = http::send("Model provider", catalog_request(&provider, key.as_deref())).await?;
    let rows = parse_models(&provider, &value);
    if rows.is_empty() {
        return Err("The provider answered but reported no models. Check the base URL.".into());
    }
    let store = state.store.lock().map_err(|e| e.to_string())?;
    store.direct_models_replace(&provider.id, rows)
}

/// Cached models across all (or one) direct providers.
#[tauri::command]
pub fn direct_models_list(
    state: State<'_, crate::AppState>,
    id: Option<String>,
) -> Result<Vec<DirectModel>, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    store.direct_models(id.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{DirectModelRow, DirectProviderDraft, Store};
    use std::path::Path;

    fn store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    fn draft(label: &str) -> DirectProviderDraft {
        DirectProviderDraft {
            id: None,
            kind: "openai_compatible".into(),
            base_url: None,
            label: label.into(),
            default_model: Some("gpt-4o-mini".into()),
            enabled: true,
        }
    }

    #[test]
    fn save_list_delete_roundtrip() {
        let store = store();
        let saved = store.direct_provider_save(draft("OpenRouter")).unwrap();
        assert_eq!(saved.label, "OpenRouter");
        assert!(saved.enabled);
        let updated = store
            .direct_provider_save(DirectProviderDraft {
                id: Some(saved.id.clone()),
                label: "OpenRouter main".into(),
                ..draft("x")
            })
            .unwrap();
        assert_eq!(updated.label, "OpenRouter main");
        assert_eq!(store.direct_providers().unwrap().len(), 1);
        store.direct_provider_delete(&saved.id).unwrap();
        assert!(store.direct_providers().unwrap().is_empty());
        assert!(store.direct_provider_delete(&saved.id).is_err());
    }

    #[test]
    fn rejects_unknown_kind_and_empty_label() {
        let store = store();
        let mut bad = draft("x");
        bad.kind = "azure".into();
        assert!(store.direct_provider_save(bad).is_err());
        assert!(store.direct_provider_save(draft("  ")).is_err());
    }

    #[test]
    fn models_roundtrip_and_cascade() {
        let store = store();
        let provider = store.direct_provider_save(draft("P")).unwrap();
        let rows = vec![DirectModelRow {
            model_id: "gpt-4o-mini".into(),
            label: "GPT-4o mini".into(),
            context_window: Some(128_000),
            supports_tools: true,
            input_price_per_mtok: Some(150),
            output_price_per_mtok: Some(600),
        }];
        let models = store.direct_models_replace(&provider.id, rows).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_window, Some(128_000));
        assert_eq!(store.direct_models(None).unwrap().len(), 1);
        assert_eq!(store.direct_models(Some(&provider.id)).unwrap().len(), 1);
        store.direct_provider_delete(&provider.id).unwrap();
        assert!(store.direct_models(None).unwrap().is_empty(), "models cascade with their provider");
    }

    #[test]
    fn openrouter_catalog_parses_with_pricing() {
        let provider = DirectProvider {
            id: "p".into(),
            kind: "openai_compatible".into(),
            base_url: None,
            label: "p".into(),
            default_model: None,
            enabled: true,
            created_at: 0,
            updated_at: 0,
        };
        let value: serde_json::Value = serde_json::json!({
            "data": [{
                "id": "anthropic/claude-3.5-sonnet",
                "name": "Claude 3.5 Sonnet",
                "context_length": 200000,
                "pricing": {"prompt": "0.000003", "completion": "0.000015"}
            }]
        });
        let rows = parse_models(&provider, &value);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model_id, "anthropic/claude-3.5-sonnet");
        assert_eq!(rows[0].context_window, Some(200_000));
        assert_eq!(rows[0].input_price_per_mtok, Some(300)); // $3 / Mtok in cents
        assert_eq!(rows[0].output_price_per_mtok, Some(1_500));
    }

    #[tokio::test]
    async fn discovery_and_test_against_a_mock() {
        let (base, seen) = http::mock::serve(|request, _| {
            assert!(request.authorization.as_deref().unwrap_or_default().starts_with("Bearer "));
            (200, r#"{"data":[{"id":"m1","name":"Model One"},{"id":"m2"}]}"#.as_bytes().to_vec())
        });
        let provider = DirectProvider {
            id: "p".into(),
            kind: "openai_compatible".into(),
            base_url: Some(base.clone()),
            label: "p".into(),
            default_model: None,
            enabled: true,
            created_at: 0,
            updated_at: 0,
        };
        let value = http::send(
            "Model provider",
            catalog_request(&provider, Some("test-key")),
        )
        .await
        .unwrap();
        let rows = parse_models(&provider, &value);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model_id, "m1");
        assert_eq!(rows[1].label, "m2");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(api_base(&provider), base);
    }

    #[tokio::test]
    async fn test_reports_auth_failures_readably() {
        let (base, _) = http::mock::serve(|_, _| (401, r#"{"error":{"message":"bad key"}}"#.as_bytes().to_vec()));
        let provider = DirectProvider {
            id: "p".into(),
            kind: "openai_compatible".into(),
            base_url: Some(base),
            label: "p".into(),
            default_model: None,
            enabled: true,
            created_at: 0,
            updated_at: 0,
        };
        let outcome = http::send("Model provider", catalog_request(&provider, Some("nope"))).await;
        let error = outcome.unwrap_err();
        assert!(error.contains("refused the credentials"), "readable error: {error}");
    }
}
