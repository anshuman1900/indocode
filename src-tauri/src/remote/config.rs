//! Build configuration, the persisted device settings and relay token claims.
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Keychain service for the relay token and the device's X25519 private key.
pub(crate) const SERVICE: &str = "app.indocode.remote";

/// Keychain account names for one install, scoped by its data folder. A dev build and the
/// installed app (each its own device) must not share one item: macOS ties an item to the
/// signature that made it, so the other build was refused access (errSecAuthFailed, -25293),
/// and one build's sign-in overwrote the other's token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Accounts {
    pub token: String,
    pub key: String,
}

pub(crate) fn accounts(dir: &Path) -> Accounts {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(dir.to_string_lossy().as_bytes());
    let id: String = digest[..6].iter().map(|byte| format!("{byte:02x}")).collect();
    Accounts { token: format!("relay-token:{id}"), key: format!("device-key:{id}") }
}

/// The unscoped account names used before 0.3.20, adopted once by the installed app.
pub(crate) fn legacy_accounts() -> Accounts {
    Accounts { token: "relay-token".into(), key: "device-key".into() }
}

/// Only the installed app (data folder named after its identifier) ever owned the legacy
/// items; a dev build must never take them.
pub(crate) fn default_install(dir: &Path) -> bool {
    dir.file_name().is_some_and(|name| name == "ai.indocode.desktop")
}

/// Legacy values to copy to the scoped names: the token, and the key when it differs
/// from the scoped one (the legacy key is the one the relay registered for this device).
#[derive(Debug, PartialEq)]
pub(crate) struct Adoption {
    pub token: String,
    pub key: Option<String>,
}

/// Whether to adopt the legacy items. `legacy` is read only when a scoped item is missing
/// on the installed app, so other installs never touch (or get prompted for) them.
pub(crate) fn adoption(dir: &Path, device_id: &str, now: i64, token: &crate::Result<Option<String>>, key: &crate::Result<Option<String>>, legacy: impl FnOnce() -> (Option<String>, Option<String>)) -> Option<Adoption> {
    if !default_install(dir) || !(matches!(token, Ok(None)) || matches!(key, Ok(None))) {
        return None;
    }
    let (legacy_token, legacy_key) = legacy();
    let legacy_token = legacy_token.filter(|t| token_valid(t, now) && parse_token(t).is_some_and(|claims| claims.dev == device_id))?;
    let legacy_key = legacy_key.filter(|k| super::crypto::decode_secret(k).is_some() && key.as_ref().ok().and_then(Option::as_deref) != Some(k.as_str()));
    Some(Adoption { token: legacy_token, key: legacy_key })
}

/// How a signed-in computer starts: only a real not-found (or an unusable token) signs out;
/// a keychain error keeps the account and is retried.
#[derive(Debug, PartialEq)]
pub(crate) enum Startup {
    SignedIn(String),
    SignedOut,
    Unreadable,
}

pub(crate) fn startup(read: crate::Result<Option<String>>, now: i64) -> Startup {
    match read {
        Ok(Some(token)) if token_valid(&token, now) => Startup::SignedIn(token),
        Ok(_) => Startup::SignedOut,
        Err(_) => Startup::Unreadable,
    }
}

/// Refresh once fewer than 7 days are left of the token's 30.
pub(crate) fn needs_refresh(token: &str, now: i64) -> bool {
    parse_token(token).is_some_and(|claims| claims.exp - now < 7 * 24 * 3600)
}
const FILE: &str = "remote.json";

/// The relay and GitHub OAuth app this build talks to. Missing either means "not configured".
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Build {
    pub relay: String,
    pub client_id: String,
}

fn setting(name: &str, built: Option<&'static str>) -> Option<String> {
    std::env::var(name).ok().or(built.map(str::to_owned)).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

pub(crate) fn build() -> Option<Build> {
    parse_build(setting("INDOCODE_RELAY_URL", option_env!("INDOCODE_RELAY_URL")), setting("INDOCODE_GITHUB_CLIENT_ID", option_env!("INDOCODE_GITHUB_CLIENT_ID")))
}

pub(crate) fn parse_build(relay: Option<String>, client_id: Option<String>) -> Option<Build> {
    let relay = relay?.trim_end_matches('/').to_owned();
    let client_id = client_id?;
    // Only HTTPS relays: the token rides every request.
    let host = relay.strip_prefix("https://")?;
    if host.is_empty() || host.contains(char::is_whitespace) || !client_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return None;
    }
    Some(Build { relay, client_id })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Account {
    pub id: String,
    pub login: String,
    #[serde(default)]
    pub avatar_url: String,
}

/// Non-secret state in the app data dir. The token and private key stay in the keychain.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub host_enabled: bool,
    #[serde(default)]
    pub account: Option<Account>,
    /// Devices allowed to drive this one: device id → the public key pinned when trusted.
    #[serde(default)]
    pub trusted: std::collections::BTreeMap<String, String>,
}

impl Settings {
    /// Trusted only while the pinned key is still the device's current key.
    pub(crate) fn is_trusted(&self, device_id: &str, current_key: &str) -> bool {
        self.trusted.get(device_id).is_some_and(|pinned| !pinned.is_empty() && pinned == current_key)
    }
}

/// Reads the settings, creating the device id once.
pub(crate) fn load(dir: &Path) -> Settings {
    let path = dir.join(FILE);
    let mut settings: Settings = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    if uuid::Uuid::parse_str(&settings.device_id).is_err() {
        settings.device_id = uuid::Uuid::new_v4().to_string();
        if let Err(error) = save(dir, &settings) {
            eprintln!("Could not save the remote access device id: {error}");
        }
    }
    settings
}

pub(crate) fn save(dir: &Path, settings: &Settings) -> crate::Result<()> {
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let temp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&temp, text).and_then(|_| std::fs::rename(&temp, dir.join(FILE))).map_err(|e| format!("Could not save remote access settings: {e}"))
}

/// The device key as stored: `Ok(None)` only when there is none. A keychain failure (a denied
/// or dismissed prompt, a locked keychain) is an error, so the key is never replaced over it.
pub(crate) fn read_key(account: &str) -> crate::Result<Option<String>> {
    #[cfg(test)]
    {
        Ok(crate::secret_store::get(SERVICE, account))
    }
    #[cfg(all(not(test), target_os = "macos"))]
    {
        match security_framework::passwords::get_generic_password(SERVICE, account) {
            Ok(bytes) => Ok(String::from_utf8(bytes).ok()),
            Err(error) if error.code() == -25300 => Ok(None),
            // -25293 (auth failed) and -128 (cancelled): the access prompt was denied or dismissed.
            Err(error) if matches!(error.code(), -25293 | -128) => Err("macOS did not let Indocode read its Connect key. Choose Always Allow when Keychain asks, or sign out and sign in again.".into()),
            Err(error) => Err(format!("Could not read this computer's Indocode Connect key from Keychain ({}).", error.code())),
        }
    }
    #[cfg(all(not(test), any(target_os = "linux", target_os = "windows")))]
    {
        match keyring::Entry::new(SERVICE, account).and_then(|entry| entry.get_password()) {
            Ok(text) => Ok(Some(text)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err("Could not read this computer's Indocode Connect key from secure storage.".into()),
        }
    }
    #[cfg(all(not(test), not(any(target_os = "macos", target_os = "linux", target_os = "windows"))))]
    {
        Err("Secure storage is not available on this platform.".into())
    }
}

/// The stored key, or a new one written once when none (or an unreadable value) is stored.
/// A read error is returned as is.
pub(crate) fn load_or_create_key(stored: crate::Result<Option<String>>, write: impl FnOnce(&str) -> crate::Result<()>) -> crate::Result<x25519_dalek::StaticSecret> {
    if let Some(secret) = stored?.and_then(|text| super::crypto::decode_secret(&text)) {
        return Ok(secret);
    }
    let secret = super::crypto::generate();
    write(&super::crypto::encode_secret(&secret))?;
    Ok(secret)
}

/// The payload half of a relay token. The signature is the relay's to check.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct Claims {
    pub acct: String,
    pub dev: String,
    pub iat: i64,
    pub exp: i64,
}

pub(crate) fn parse_token(token: &str) -> Option<Claims> {
    let (payload, signature) = token.split_once('.')?;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    if signature.is_empty() || signature.contains('.') || engine.decode(signature).is_err() {
        return None;
    }
    let claims: Claims = serde_json::from_slice(&engine.decode(payload).ok()?).ok()?;
    (claims.exp > claims.iat && !claims.acct.is_empty() && !claims.dev.is_empty()).then_some(claims)
}

/// A token is usable while it is unexpired (`exp` in seconds).
pub(crate) fn token_valid(token: &str, now_seconds: i64) -> bool {
    parse_token(token).is_some_and(|claims| claims.exp > now_seconds)
}

pub(crate) fn device_name() -> String {
    #[cfg(unix)]
    let name = {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is valid for its length, and gethostname NUL-terminates within it.
        let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
        let end = buffer.iter().position(|&b| b == 0).unwrap_or(0);
        ok.then(|| String::from_utf8_lossy(&buffer[..end]).into_owned())
    };
    #[cfg(not(unix))]
    let name = std::env::var("COMPUTERNAME").ok();
    let name = name.unwrap_or_default();
    let name = name.trim().trim_end_matches(".local").chars().filter(|c| !c.is_control()).take(64).collect::<String>();
    if name.is_empty() { "This computer".into() } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(payload: &str) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}", engine.encode(payload), engine.encode([7u8; 32]))
    }

    #[test]
    fn tokens_parse_and_expire() {
        let good = token(r#"{"acct":"gh:1","dev":"d1","iat":100,"exp":200}"#);
        assert_eq!(parse_token(&good), Some(Claims { acct: "gh:1".into(), dev: "d1".into(), iat: 100, exp: 200 }));
        assert!(token_valid(&good, 150));
        assert!(!token_valid(&good, 200));
        assert_eq!(parse_token("nodot"), None);
        assert_eq!(parse_token(&format!("{}.", token(r#"{"acct":"a","dev":"d","iat":1,"exp":2}"#).split('.').next().unwrap())), None);
        assert_eq!(parse_token(&token(r#"{"acct":"","dev":"d","iat":1,"exp":2}"#)), None);
        assert_eq!(parse_token(&token("not json")), None);
    }

    #[test]
    fn legacy_items_are_adopted_only_when_they_belong_here() {
        let installed = Path::new("/Users/me/Library/Application Support/ai.indocode.desktop");
        let good = token(r#"{"acct":"gh:1","dev":"d1","iat":100,"exp":200}"#);
        let legacy_key = super::super::crypto::encode_secret(&super::super::crypto::generate());
        let scoped_key = super::super::crypto::encode_secret(&super::super::crypto::generate());
        let legacy = |t: &String, k: &String| {
            let pair = (Some(t.clone()), Some(k.clone()));
            move || pair
        };
        // Missing scoped token: adopt both, the legacy key replacing a different scoped key.
        assert_eq!(adoption(installed, "d1", 150, &Ok(None), &Ok(Some(scoped_key.clone())), legacy(&good, &legacy_key)), Some(Adoption { token: good.clone(), key: Some(legacy_key.clone()) }));
        // The same key is not written again.
        assert_eq!(adoption(installed, "d1", 150, &Ok(None), &Ok(Some(legacy_key.clone())), legacy(&good, &legacy_key)), Some(Adoption { token: good.clone(), key: None }));
        // Another device's token, an expired one, or a dev build: nothing.
        assert_eq!(adoption(installed, "d2", 150, &Ok(None), &Ok(None), legacy(&good, &legacy_key)), None);
        assert_eq!(adoption(installed, "d1", 200, &Ok(None), &Ok(None), legacy(&good, &legacy_key)), None);
        assert_eq!(adoption(Path::new("/Users/me/Library/Application Support/ai.indocode.dev"), "d1", 150, &Ok(None), &Ok(None), || panic!("a dev build read the legacy items")), None);
        // Both scoped items present, or the token unreadable: the legacy items are never read.
        assert_eq!(adoption(installed, "d1", 150, &Ok(Some(good.clone())), &Ok(Some(scoped_key)), || panic!("read")), None);
        assert_eq!(adoption(installed, "d1", 150, &Err("locked".into()), &Err("locked".into()), || panic!("read")), None);
        assert_eq!(adoption(installed, "d1", 150, &Ok(None), &Ok(None), || (None, None)), None);
    }

    #[test]
    fn only_a_missing_token_signs_out_at_startup() {
        let good = token(r#"{"acct":"gh:1","dev":"d1","iat":100,"exp":200}"#);
        assert_eq!(startup(Ok(Some(good.clone())), 150), Startup::SignedIn(good.clone()));
        assert_eq!(startup(Ok(Some(good)), 250), Startup::SignedOut);
        assert_eq!(startup(Ok(None), 150), Startup::SignedOut);
        assert_eq!(startup(Err("Keychain locked (-25308)".into()), 150), Startup::Unreadable);
    }

    #[test]
    fn tokens_refresh_in_their_last_week() {
        let day = 24 * 3600;
        let t = token(&format!(r#"{{"acct":"gh:1","dev":"d1","iat":0,"exp":{}}}"#, 30 * day));
        assert!(!needs_refresh(&t, 0));
        assert!(!needs_refresh(&t, 23 * day));
        assert!(needs_refresh(&t, 23 * day + 1));
        assert!(!needs_refresh("garbage", 0));
    }

    #[test]
    fn a_keychain_error_never_replaces_the_key() {
        let written = std::cell::Cell::new(false);
        let write = |_: &str| {
            written.set(true);
            Ok(())
        };
        assert!(load_or_create_key(Err("Keychain locked".into()), write).is_err());
        assert!(!written.get());
        let existing = super::super::crypto::generate();
        let kept = load_or_create_key(Ok(Some(super::super::crypto::encode_secret(&existing))), write).unwrap();
        assert_eq!(kept.to_bytes(), existing.to_bytes());
        assert!(!written.get());
        load_or_create_key(Ok(None), write).unwrap();
        assert!(written.get());
    }

    #[test]
    fn keychain_accounts_are_scoped_per_install() {
        let installed = accounts(Path::new("/Users/me/Library/Application Support/ai.indocode.desktop"));
        let dev = accounts(Path::new("/Users/me/Library/Application Support/ai.indocode.dev"));
        assert_ne!(installed, dev);
        assert_eq!(installed, accounts(Path::new("/Users/me/Library/Application Support/ai.indocode.desktop")));
        assert!(installed.key.starts_with("device-key:") && installed.token.starts_with("relay-token:"));
    }

    #[test]
    fn build_config_needs_https_and_a_client_id() {
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(parse_build(some("https://relay.example/"), some("Iv1.abc")), Some(Build { relay: "https://relay.example".into(), client_id: "Iv1.abc".into() }));
        assert_eq!(parse_build(some("http://relay.example"), some("Iv1.abc")), None);
        assert_eq!(parse_build(some("https://relay.example"), None), None);
        assert_eq!(parse_build(None, some("x")), None);
        assert_eq!(parse_build(some("https://relay.example"), some("bad id")), None);
    }

    #[test]
    fn settings_keep_one_device_id_and_default_host_off() {
        let dir = std::env::temp_dir().join(format!("indocode-remote-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = load(&dir);
        assert!(!first.host_enabled);
        assert!(first.account.is_none());
        assert_eq!(load(&dir).device_id, first.device_id);
        let parsed: Settings = serde_json::from_str(r#"{"deviceId":"x","hostEnabled":true,"account":{"id":"gh:1","login":"a","avatarUrl":"u"}}"#).unwrap();
        assert!(parsed.host_enabled);
        assert_eq!(parsed.account.unwrap().avatar_url, "u");
        assert!(parsed.trusted.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
