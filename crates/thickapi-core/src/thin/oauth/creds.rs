//! Read-only vendor credential readers (SPEC §19.8). No cache, no daemon-side refresh: the
//! CLIs own their token files and serialise writes with their own locks.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Token {
    pub access_token: String,
    /// Unix seconds; `None` when the store does not record expiry.
    pub expires_at: Option<u64>,
    pub source: PathBuf,
    /// Vendor-specific extras (subscription type, account, …) for `/ready`; never the token.
    pub note: Option<String>,
}

impl Token {
    pub fn expired_at(&self, now_unix: u64) -> bool {
        self.expires_at.is_some_and(|exp| exp <= now_unix)
    }

    pub fn expired(&self) -> bool {
        self.expired_at(now_unix())
    }

    /// Seconds until expiry (0 when expired or unknown).
    pub fn seconds_left(&self) -> u64 {
        self.expires_at
            .map(|exp| exp.saturating_sub(now_unix()))
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum CredError {
    #[error("credential file not found: {0}")]
    Missing(PathBuf),
    #[error("credential file {0} unreadable: {1}")]
    Unreadable(PathBuf, String),
    #[error("credential file {0} malformed: {1}")]
    Malformed(PathBuf, String),
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn read_json(path: &Path) -> Result<Value, CredError> {
    if !path.exists() {
        return Err(CredError::Missing(path.to_path_buf()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| CredError::Unreadable(path.to_path_buf(), e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| CredError::Malformed(path.to_path_buf(), e.to_string()))
}

/// RFC 3339 (any offset, nanosecond fractions welcome) → Unix seconds.
fn rfc3339_to_unix(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp().max(0) as u64)
}

/// Milliseconds or seconds → Unix seconds.
fn unix_seconds(v: &Value) -> Option<u64> {
    let n = v.as_f64()?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    Some(if n >= 100_000_000_000.0 {
        (n / 1000.0) as u64
    } else {
        n as u64
    })
}

/// `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR/.credentials.json`).
pub fn claude_credentials_path() -> PathBuf {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude"));
    dir.join(".credentials.json")
}

pub fn claude_token() -> Result<Token, CredError> {
    claude_token_from(&claude_credentials_path())
}

pub fn claude_token_from(path: &Path) -> Result<Token, CredError> {
    let doc = read_json(path)?;
    let oauth = doc
        .get("claudeAiOauth")
        .ok_or_else(|| CredError::Malformed(path.to_path_buf(), "missing claudeAiOauth".into()))?;
    let access_token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CredError::Malformed(
                path.to_path_buf(),
                "missing claudeAiOauth.accessToken".into(),
            )
        })?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(unix_seconds);
    let note = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .map(|s| format!("subscription={s}"));
    Ok(Token {
        access_token,
        expires_at,
        source: path.to_path_buf(),
        note,
    })
}

/// `~/.grok/auth.json` (grok CLI): a map keyed by `<oidc_issuer>::<oidc_client_id>` whose
/// entries carry `key` (a JWT bearer), `auth_mode`, `expires_at` (RFC 3339), `refresh_token`.
pub fn grok_credentials_path() -> PathBuf {
    home().join(".grok").join("auth.json")
}

pub fn grok_token() -> Result<Token, CredError> {
    grok_token_from(&grok_credentials_path())
}

/// Issuer thickAPI accepts; a rotated issuer is a visible `/ready` failure, not a silent retry.
pub const GROK_ISSUER: &str = "https://auth.x.ai";

pub fn grok_token_from(path: &Path) -> Result<Token, CredError> {
    let doc = read_json(path)?;
    let entries = doc.as_object().ok_or_else(|| {
        CredError::Malformed(path.to_path_buf(), "top level is not an object".into())
    })?;
    // Prefer the x.ai issuer; otherwise the newest entry that has a key.
    let mut chosen: Option<(&String, &Value)> = None;
    for (k, v) in entries {
        if v.get("key").and_then(Value::as_str).is_none() {
            continue;
        }
        let is_xai = k.starts_with(GROK_ISSUER)
            || v.get("oidc_issuer").and_then(Value::as_str) == Some(GROK_ISSUER);
        match chosen {
            None => chosen = Some((k, v)),
            Some((ck, _)) if is_xai && !ck.starts_with(GROK_ISSUER) => chosen = Some((k, v)),
            _ => {}
        }
    }
    let (_, entry) = chosen
        .ok_or_else(|| CredError::Malformed(path.to_path_buf(), "no entry with a `key`".into()))?;
    let access_token = entry["key"].as_str().unwrap_or_default().to_string();
    let expires_at = entry.get("expires_at").and_then(|v| match v {
        Value::String(s) => rfc3339_to_unix(s),
        other => unix_seconds(other),
    });
    let issuer = entry
        .get("oidc_issuer")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mode = entry
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Ok(Token {
        access_token,
        expires_at,
        source: path.to_path_buf(),
        note: Some(format!("issuer={issuer} auth_mode={mode}")),
    })
}

/// `~/.gemini/oauth_creds.json` — gemini-cli's store, **not** Antigravity's (kept for the
/// flat-shape fallback in [`antigravity_token_from_json`]).
pub fn gemini_credentials_path() -> PathBuf {
    home().join(".gemini").join("oauth_creds.json")
}

pub fn gemini_token() -> Result<Token, CredError> {
    gemini_token_from(&gemini_credentials_path())
}

pub fn gemini_token_from(path: &Path) -> Result<Token, CredError> {
    let doc = read_json(path)?;
    antigravity_token_from_json(&doc, path)
}

/// Windows Credential Manager target the `agy` CLI (1.2.4) writes its Google OAuth token to
/// (`keyring.go`: generic credential, user `antigravity`, blob = UTF-8 JSON).
pub const ANTIGRAVITY_CRED_TARGET: &str = "gemini:antigravity";

/// Where the `agy` CLI keeps its token on non-Windows hosts.
pub fn antigravity_credentials_path() -> PathBuf {
    home()
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token")
}

/// The `agy` CLI's stored token: Windows Credential Manager first, then the file store.
pub fn antigravity_token() -> Result<Token, CredError> {
    #[cfg(windows)]
    {
        match super::wincred::read_generic(ANTIGRAVITY_CRED_TARGET) {
            Ok(blob) => {
                let source = PathBuf::from(format!("credman:{ANTIGRAVITY_CRED_TARGET}"));
                let doc: Value = serde_json::from_slice(&blob)
                    .map_err(|e| CredError::Malformed(source.clone(), e.to_string()))?;
                return antigravity_token_from_json(&doc, &source);
            }
            Err(super::wincred::WinCredError::NotFound) => {}
            Err(e) => {
                return Err(CredError::Unreadable(
                    PathBuf::from(format!("credman:{ANTIGRAVITY_CRED_TARGET}")),
                    e.to_string(),
                ))
            }
        }
    }
    antigravity_token_from(&antigravity_credentials_path())
}

pub fn antigravity_token_from(path: &Path) -> Result<Token, CredError> {
    let doc = read_json(path)?;
    antigravity_token_from_json(&doc, path)
}

/// Accepts the `agy` shape `{"token":{"access_token","expiry":<RFC3339>,…},"auth_method",
/// "id_token"}` and the flat gemini-cli shape `{"access_token","expiry_date":<ms>}`.
pub fn antigravity_token_from_json(doc: &Value, source: &Path) -> Result<Token, CredError> {
    let inner = doc.get("token").filter(|t| t.is_object()).unwrap_or(doc);
    let access_token = inner
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CredError::Malformed(source.to_path_buf(), "missing access_token".into()))?
        .to_string();
    let expires_at = inner
        .get("expiry")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_unix)
        .or_else(|| inner.get("expiry_date").and_then(unix_seconds))
        .or_else(|| inner.get("expiry").and_then(unix_seconds));
    let mut notes = Vec::new();
    if let Some(m) = doc.get("auth_method").and_then(Value::as_str) {
        notes.push(format!("auth_method={m}"));
    }
    if doc
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_email)
        .is_some()
    {
        notes.push("account=<redacted>".to_string());
    }
    Ok(Token {
        access_token,
        expires_at,
        source: source.to_path_buf(),
        note: if notes.is_empty() {
            None
        } else {
            Some(notes.join(" "))
        },
    })
}

/// `cursor-agent`'s token store (measured T4, CLI `2026.09.15-d2fe57e`): `getAuthFilePath()` is
/// `%APPDATA%\Cursor\auth.json` on Windows, `~/.cursor/auth.json` on macOS and
/// `$XDG_CONFIG_HOME/cursor/auth.json` (default `~/.config/cursor/auth.json`) on Linux.
/// `CURSOR_CONFIG_DIR` overrides the directory. Shape: `{"accessToken","refreshToken"}` — both
/// `authentication.cursor.sh` session JWTs (`type: session`, `exp` ≈ 60 days out).
pub fn cursor_credentials_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("CURSOR_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("auth.json");
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|d| !d.is_empty()) {
            return PathBuf::from(appdata).join("Cursor").join("auth.json");
        }
    }
    #[cfg(target_os = "macos")]
    {
        return home().join(".cursor").join("auth.json");
    }
    #[allow(unreachable_code)]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"));
        base.join("cursor").join("auth.json")
    }
}

pub fn cursor_token() -> Result<Token, CredError> {
    cursor_token_from(&cursor_credentials_path())
}

pub fn cursor_token_from(path: &Path) -> Result<Token, CredError> {
    let doc = read_json(path)?;
    let access_token = doc
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CredError::Malformed(path.to_path_buf(), "missing accessToken".into()))?
        .to_string();
    let claims = jwt_claims(&access_token);
    let expires_at = claims
        .as_ref()
        .and_then(|c| c.get("exp"))
        .and_then(unix_seconds);
    let mut notes = Vec::new();
    if let Some(c) = &claims {
        if let Some(t) = c.get("type").and_then(Value::as_str) {
            notes.push(format!("type={t}"));
        }
        if let Some(sub) = c.get("sub").and_then(Value::as_str) {
            // `google-oauth2|user_…` → the provider only; the user id is not for `/ready`.
            let provider = sub.split('|').next().unwrap_or("unknown");
            notes.push(format!("idp={provider}"));
        }
    }
    Ok(Token {
        access_token,
        expires_at,
        source: path.to_path_buf(),
        note: if notes.is_empty() {
            None
        } else {
            Some(notes.join(" "))
        },
    })
}

/// `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`: the codex CLI's ChatGPT login.
pub fn codex_credentials_path() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
        .join("auth.json")
}

pub fn codex_token() -> Result<Token, CredError> {
    codex_token_from(&codex_credentials_path())
}

/// The JWT claim object the ChatGPT account and plan live under.
const CODEX_AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// `{auth_mode: "chatgpt", tokens: {access_token, refresh_token, id_token, account_id},
/// last_refresh}` (codex 0.164). The access token is a JWT whose `exp` is the expiry
/// (about ten days). An API-key login (`auth_mode: "apikey"`) is not a subscription and is
/// refused.
pub fn codex_token_from(path: &Path) -> Result<Token, CredError> {
    let doc = read_json(path)?;
    let mode = doc
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if mode != "chatgpt" {
        return Err(CredError::Malformed(
            path.to_path_buf(),
            format!("auth_mode {mode}: thickAPI serves the ChatGPT login (`codex login`), not an API key"),
        ));
    }
    let access_token = doc
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CredError::Malformed(path.to_path_buf(), "missing tokens.access_token".into())
        })?
        .to_string();
    let claims = jwt_claims(&access_token);
    let expires_at = claims
        .as_ref()
        .and_then(|c| c.get("exp"))
        .and_then(unix_seconds);
    let mut notes = vec![format!("auth_mode={mode}")];
    if let Some(plan) = claims
        .as_ref()
        .and_then(|c| {
            c.pointer(&format!(
                "/{}/chatgpt_plan_type",
                json_pointer_escape(CODEX_AUTH_CLAIM)
            ))
        })
        .and_then(Value::as_str)
    {
        notes.push(format!("plan={plan}"));
    }
    Ok(Token {
        access_token,
        expires_at,
        source: path.to_path_buf(),
        note: Some(notes.join(" ")),
    })
}

/// The ChatGPT account an access token belongs to, for the `chatgpt-account-id` header.
pub fn codex_account_id(access_token: &str) -> Option<String> {
    jwt_claims(access_token)?
        .get(CODEX_AUTH_CLAIM)?
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn json_pointer_escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Payload claims of an unverified JWT (display/expiry only; never trusted for auth).
fn jwt_claims(jwt: &str) -> Option<Value> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `email` claim of an unverified JWT (display only; never trusted for auth).
fn jwt_email(jwt: &str) -> Option<String> {
    jwt_claims(jwt)?
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn claude_credentials_parse_and_expiry_in_ms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-x","refreshToken":"r","expiresAt":1789619151920,"subscriptionType":"max"}}"#,
        )
        .unwrap();
        let token = claude_token_from(&path).unwrap();
        assert_eq!(token.access_token, "sk-ant-oat01-x");
        assert_eq!(token.expires_at, Some(1_789_619_151));
        assert_eq!(token.note.as_deref(), Some("subscription=max"));
        assert!(!token.expired_at(1_789_619_000));
        assert!(token.expired_at(1_789_619_151));
    }

    #[test]
    fn missing_and_malformed_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(matches!(
            claude_token_from(&missing),
            Err(CredError::Missing(_))
        ));
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, r#"{"claudeAiOauth":{}}"#).unwrap();
        assert!(matches!(
            claude_token_from(&bad),
            Err(CredError::Malformed(..))
        ));
    }

    #[test]
    fn grok_credentials_pick_xai_entry_and_parse_rfc3339_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"https://other::c1":{"key":"eyJother","auth_mode":"oidc","expires_at":"2026-09-17T02:25:52.858728Z","oidc_issuer":"https://other"},
                "https://auth.x.ai::b1a0":{"key":"eyJxai","auth_mode":"oidc","expires_at":"2026-09-17T02:25:52.858728Z","oidc_issuer":"https://auth.x.ai"}}"#,
        )
        .unwrap();
        let token = grok_token_from(&path).unwrap();
        assert_eq!(token.access_token, "eyJxai");
        assert_eq!(token.expires_at, Some(1_789_611_952));
        assert!(token
            .note
            .as_deref()
            .unwrap()
            .contains("issuer=https://auth.x.ai"));
    }

    #[test]
    fn gemini_credentials_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_creds.json");
        std::fs::write(
            &path,
            r#"{"access_token":"ya29.x","refresh_token":"r","expiry_date":1789619151920}"#,
        )
        .unwrap();
        let token = gemini_token_from(&path).unwrap();
        assert_eq!(token.access_token, "ya29.x");
        assert_eq!(token.expires_at, Some(1_789_619_151));
    }

    #[test]
    fn antigravity_keyring_shape_parses_rfc3339_expiry_and_account() {
        // `id_token` payload: {"email":"e2e@example.com"}
        let payload = {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(r#"{"email":"e2e@example.com"}"#)
        };
        let doc: Value = serde_json::from_str(&format!(
            r#"{{"token":{{"access_token":"ya29.agy","token_type":"Bearer","refresh_token":"1//r","expiry":"2026-09-17T02:28:22.487930+00:00"}},"auth_method":"consumer","id_token":"h.{payload}.s"}}"#
        ))
        .unwrap();
        let token =
            antigravity_token_from_json(&doc, Path::new("credman:gemini:antigravity")).unwrap();
        assert_eq!(token.access_token, "ya29.agy");
        assert_eq!(token.expires_at, Some(1_789_612_102));
        assert_eq!(
            token.note.as_deref(),
            Some("auth_method=consumer account=<redacted>")
        );
        assert!(!token.note.as_deref().unwrap().contains('@'));
    }

    #[test]
    fn cursor_auth_json_reads_jwt_expiry_and_hides_the_user_id() {
        use base64::Engine;
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            r#"{"sub":"google-oauth2|user_01ABC","exp":1794343349,"iss":"https://authentication.cursor.sh","type":"session"}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            format!(r#"{{"accessToken":"h.{claims}.s","refreshToken":"h.{claims}.s"}}"#),
        )
        .unwrap();
        let token = cursor_token_from(&path).unwrap();
        assert_eq!(token.access_token, format!("h.{claims}.s"));
        assert_eq!(token.expires_at, Some(1_794_343_349));
        assert_eq!(
            token.note.as_deref(),
            Some("type=session idp=google-oauth2")
        );
        assert!(!token.note.as_deref().unwrap().contains("user_01ABC"));
    }

    #[test]
    fn codex_auth_json_reads_jwt_expiry_plan_and_account() {
        use base64::Engine;
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            r#"{"exp":1793522400,"https://api.openai.com/auth":{"chatgpt_account_id":"acc-1","chatgpt_plan_type":"plus"}}"#,
        );
        let jwt = format!("h.{claims}.s");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"id_token":"x","access_token":"{jwt}","refresh_token":"r","account_id":"acc-1"}},"last_refresh":"2026-10-22T08:48:19Z"}}"#
            ),
        )
        .unwrap();
        let token = codex_token_from(&path).unwrap();
        assert_eq!(token.access_token, jwt);
        assert_eq!(token.expires_at, Some(1_793_522_400));
        assert_eq!(token.note.as_deref(), Some("auth_mode=chatgpt plan=plus"));
        assert_eq!(codex_account_id(&jwt).as_deref(), Some("acc-1"));
        std::fs::write(&path, r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#).unwrap();
        assert!(matches!(
            codex_token_from(&path),
            Err(CredError::Malformed(..))
        ));
    }
}
