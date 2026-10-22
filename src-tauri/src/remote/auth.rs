//! GitHub device-flow sign-in and the relay's HTTP API.
use super::config::{Account, Build};
use crate::{cloud::http, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Returned when the relay refuses the token: the device was revoked or the token expired.
pub(crate) const SIGNED_OUT: &str = "This computer was signed out of Indocode Connect.";
/// Sign-in refused because this device id is registered with another key (HTTP 409).
pub(crate) const KEY_CHANGED: &str = "The relay has another key for this device.";
const GITHUB: &str = "https://github.com";
const LIMIT: usize = 1024 * 1024;

pub(crate) struct Flow {
    device_code: String,
    interval: u64,
    expires: Instant,
}

/// Form bodies here carry only validated ASCII tokens, so no escaping is needed.
fn safe(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

async fn github_form(path: &str, body: String) -> Result<Value> {
    let request = http::client(Duration::from_secs(20))?
        .post(format!("{GITHUB}{path}"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body);
    http::send("GitHub", request).await
}

pub(crate) async fn start(build: &Build) -> Result<(Flow, Value)> {
    let reply = github_form("/login/device/code", format!("client_id={}&scope=", build.client_id)).await?;
    let text = |key: &str| reply[key].as_str().map(str::to_owned).ok_or_else(|| format!("GitHub did not start sign-in: {}", reply["error_description"].as_str().unwrap_or("unexpected reply")));
    let (device_code, user_code, verification_uri) = (text("device_code")?, text("user_code")?, text("verification_uri")?);
    if !safe(&device_code) || !verification_uri.starts_with("https://github.com/") {
        return Err("GitHub returned an unexpected sign-in code.".into());
    }
    let expires_in = reply["expires_in"].as_u64().unwrap_or(900).min(3600);
    let flow = Flow { device_code, interval: reply["interval"].as_u64().unwrap_or(5).clamp(1, 60), expires: Instant::now() + Duration::from_secs(expires_in) };
    Ok((flow, json!({"userCode": user_code, "verificationUri": verification_uri, "expiresIn": expires_in})))
}

/// Waits for the user to approve and returns the GitHub token (kept only in memory).
pub(crate) async fn github_token(build: &Build, mut flow: Flow) -> Result<String> {
    let github_token = loop {
        if Instant::now() >= flow.expires {
            return Err("The sign-in code expired. Start again.".into());
        }
        tokio::time::sleep(Duration::from_secs(flow.interval)).await;
        let reply = github_form(
            "/login/oauth/access_token",
            format!("client_id={}&device_code={}&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code", build.client_id, flow.device_code),
        )
        .await?;
        if let Some(token) = reply["access_token"].as_str() {
            break token.to_owned();
        }
        match reply["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => flow.interval = (flow.interval + 5).min(60),
            Some("expired_token") => return Err("The sign-in code expired. Start again.".into()),
            Some("access_denied") => return Err("Sign-in was cancelled on GitHub.".into()),
            _ => return Err(format!("GitHub sign-in failed: {}", reply["error_description"].as_str().or(reply["error"].as_str()).unwrap_or("unexpected reply"))),
        }
    };
    Ok(github_token)
}

/// Trades the GitHub token for a relay token. A 409 comes back as [`KEY_CHANGED`].
pub(crate) async fn register(build: &Build, github_token: &str, device: Value) -> Result<(String, Account)> {
    let reply = request(build, None, reqwest::Method::POST, "/v1/auth/github", Some(json!({"githubToken": github_token, "device": device}))).await?;
    let token = reply["token"].as_str().filter(|token| token.len() <= 4096).ok_or("The relay did not return a token.")?.to_owned();
    let account = serde_json::from_value::<Account>(reply["account"].clone()).map_err(|_| "The relay returned an unreadable account.")?;
    Ok((token, account))
}

/// An authenticated relay call. A refused token comes back as [`SIGNED_OUT`].
pub(crate) async fn relay(build: &Build, token: &str, method: reqwest::Method, path: &str) -> Result<Value> {
    request(build, Some(token), method, path, None).await
}

async fn request(build: &Build, token: Option<&str>, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
    let mut request = http::client(Duration::from_secs(20))?.request(method, format!("{}{path}", build.relay));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.header("Content-Type", "application/json").body(body.to_string());
    }
    let response = request.send().await.map_err(|e| if e.is_timeout() { "The relay did not answer in time.".to_string() } else { "Could not reach the relay. Check your connection.".to_string() })?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(|_| "The relay's response was cut off.")?;
    if bytes.len() > LIMIT {
        return Err("The relay sent a response that is too large.".into());
    }
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    match status {
        200..=299 => Ok(value),
        401 if token.is_some() => Err(SIGNED_OUT.into()),
        409 if token.is_none() => Err(KEY_CHANGED.into()),
        _ => Err(value["error"].as_str().map(|error| format!("Relay: {}", error.chars().take(300).collect::<String>())).unwrap_or_else(|| format!("The relay returned HTTP {status}."))),
    }
}
