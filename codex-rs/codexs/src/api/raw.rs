//! Raw forward: a Codex-shaped Responses request — what Codex CLI (or anything
//! replaying its traffic) sends — goes upstream **as the client built it**:
//! its body, its headers, its telemetry. Only three things change:
//!
//! * credentials: `Authorization` / `chatgpt-account-id` become this instance's
//!   account (tokens refreshed here when expired);
//! * `<environment_context>`: `<timezone>` and `<current_date>` are rewritten
//!   to the egress IP's zone, so the request looks produced where it leaves;
//! * downstream-only headers (`x-asxs-*`, proxy/hop-by-hop, forwarded-for) are dropped.
//!
//! Nothing is bridged through the in-process Codex, so no second base prompt,
//! no second environment context. The upstream stream is relayed byte-for-byte.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use chrono::Utc;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_login::token_data::parse_jwt_expiration;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;
use tracing::warn;

use super::server::ApiError;
use crate::config::ProxyConfig;
use crate::config::RawForwardMode;
use crate::tz::EgressTimezone;

const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Refresh when the access token expires within this window.
const REFRESH_MARGIN: chrono::Duration = chrono::Duration::minutes(10);

pub struct RawForwarder {
    cfg: Arc<ProxyConfig>,
    client: reqwest::Client,
    tz: Arc<EgressTimezone>,
    refresh_lock: Mutex<()>,
    tz_re: regex::Regex,
    date_re: regex::Regex,
}

struct Account {
    id: String,
    codex_home: PathBuf,
}

struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

impl RawForwarder {
    pub fn new(cfg: Arc<ProxyConfig>, client: reqwest::Client, tz: Arc<EgressTimezone>) -> Self {
        Self {
            cfg,
            client,
            tz,
            refresh_lock: Mutex::new(()),
            tz_re: regex::Regex::new(r"<timezone>[^<]*</timezone>").expect("static regex"),
            date_re: regex::Regex::new(r"<current_date>\d{4}-\d{2}-\d{2}</current_date>")
                .expect("static regex"),
        }
    }

    /// Forward if this request should go raw; `None` = use the Codex bridge.
    pub async fn try_forward(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
        parsed: &Value,
    ) -> Result<Option<Response>, ApiError> {
        if !self.should_forward(headers, parsed) {
            return Ok(None);
        }
        let Some(account) = self.pick_account(headers) else {
            return Ok(None);
        };
        let model = parsed
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let stream = parsed
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let (body, tz) = self.prepare_body(body, parsed)?;
        dump_outgoing(&body);
        let mut tokens = self.tokens_for(&account).await?;
        let mut resp = self.send(headers, &tokens, &body, stream).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            warn!(account = %account.id, "upstream 401 on raw forward; refreshing token and retrying once");
            tokens = self.refresh(&account, &tokens).await?;
            resp = self.send(headers, &tokens, &body, stream).await?;
        }
        info!(
            account = %account.id, model = %model, stream, tz = tz.as_deref().unwrap_or("-"),
            status = resp.status().as_u16(), "raw forward"
        );
        Ok(Some(relay(resp)))
    }

    fn should_forward(&self, headers: &HeaderMap, body: &Value) -> bool {
        if let Some(v) = headers.get("x-asxs-raw").and_then(|v| v.to_str().ok()) {
            return matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "on" | "true" | "yes"
            );
        }
        match self.cfg.defaults.raw_forward {
            RawForwardMode::Always => true,
            RawForwardMode::Never => false,
            RawForwardMode::Auto => looks_like_codex_request(body),
        }
    }

    /// Raw forwarding only serves the static accounts (server mode has exactly one).
    fn pick_account(&self, headers: &HeaderMap) -> Option<Account> {
        let wanted = headers
            .get("x-asxs-account")
            .and_then(|v| v.to_str().ok())
            .map(str::trim);
        self.cfg
            .accounts
            .iter()
            .filter(|a| a.enabled)
            .find(|a| wanted.is_none_or(|w| w.is_empty() || w == a.id))
            .map(|a| Account {
                id: a.id.clone(),
                codex_home: a.codex_home.clone(),
            })
    }

    /// Localise `<environment_context>` and drop the `asxs` control object.
    fn prepare_body(
        &self,
        body: &Bytes,
        parsed: &Value,
    ) -> Result<(Bytes, Option<String>), ApiError> {
        let mut text = if parsed.get("asxs").is_some() {
            let mut v = parsed.clone();
            if let Some(o) = v.as_object_mut() {
                o.remove("asxs");
            }
            serde_json::to_string(&v).map_err(|e| ApiError::internal(e.to_string()))?
        } else {
            String::from_utf8(body.to_vec())
                .map_err(|_| ApiError::bad_request("request body is not UTF-8"))?
        };
        let tz = self.tz.current();
        if let Some(tz) = tz {
            let name = tz.name().to_string();
            text = self
                .tz_re
                .replace_all(&text, format!("<timezone>{name}</timezone>").as_str())
                .into_owned();
            if let Some(today) = self.tz.today() {
                text = self
                    .date_re
                    .replace_all(
                        &text,
                        format!("<current_date>{today}</current_date>").as_str(),
                    )
                    .into_owned();
            }
        }
        Ok((Bytes::from(text), tz.map(|t| t.name().to_string())))
    }

    async fn send(
        &self,
        headers: &HeaderMap,
        tokens: &Tokens,
        body: &Bytes,
        stream: bool,
    ) -> Result<reqwest::Response, ApiError> {
        let url = format!(
            "{}/responses",
            self.cfg.codex.chatgpt_base_url.trim_end_matches('/')
        );
        let mut out = HeaderMap::new();
        for (k, v) in headers {
            if !drop_header(k) {
                out.append(k.clone(), v.clone());
            }
        }
        out.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", tokens.access_token))
                .map_err(|_| ApiError::internal("access token is not a valid header value"))?,
        );
        if let Some(id) = &tokens.account_id
            && let Ok(v) = HeaderValue::from_str(id)
        {
            out.insert(HeaderName::from_static("chatgpt-account-id"), v);
        }
        // Defaults for a client that is not Codex itself (Codex sends all of these).
        let default = |out: &mut HeaderMap, name: &'static str, value: String| {
            if !out.contains_key(name)
                && let Ok(v) = HeaderValue::from_str(&value)
            {
                out.insert(HeaderName::from_static(name), v);
            }
        };
        default(&mut out, "content-type", "application/json".into());
        default(
            &mut out,
            "accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            }
            .into(),
        );
        default(&mut out, "originator", "codex_cli_rs".into());
        default(
            &mut out,
            "user-agent",
            format!("codex_cli_rs/{}", env!("CARGO_PKG_VERSION")),
        );
        default(&mut out, "openai-beta", "responses=experimental".into());
        default(&mut out, "session_id", uuid::Uuid::new_v4().to_string());

        self.client
            .post(&url)
            .headers(out)
            .body(body.clone())
            .send()
            .await
            .map_err(|e| ApiError::internal(format!("upstream request failed: {e}")))
    }

    // ---- account tokens -------------------------------------------------

    async fn tokens_for(&self, account: &Account) -> Result<Tokens, ApiError> {
        let tokens = read_tokens(&account.codex_home)?;
        let expiring = parse_jwt_expiration(&tokens.access_token)
            .ok()
            .flatten()
            .is_some_and(|exp| exp < Utc::now() + REFRESH_MARGIN);
        if expiring {
            info!(account = %account.id, "access token expiring; refreshing before raw forward");
            return self.refresh(account, &tokens).await;
        }
        Ok(tokens)
    }

    async fn refresh(&self, account: &Account, current: &Tokens) -> Result<Tokens, ApiError> {
        let _guard = self.refresh_lock.lock().await;
        // Another request may have refreshed while we waited.
        if let Ok(fresh) = read_tokens(&account.codex_home)
            && fresh.access_token != current.access_token
        {
            return Ok(fresh);
        }
        let Some(refresh_token) = current.refresh_token.clone().filter(|s| !s.is_empty()) else {
            return Err(ApiError::internal(format!(
                "account {} has no refresh token; run `codex login` again",
                account.id
            )));
        };
        let client_id = std::env::var("CODEX_APP_SERVER_LOGIN_CLIENT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| OAUTH_CLIENT_ID.to_string());
        let resp = self
            .client
            .post(OAUTH_TOKEN_URL)
            .timeout(Duration::from_secs(30))
            .json(&json!({
                "client_id": client_id,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .map_err(|e| ApiError::internal(format!("token refresh failed: {e}")))?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(ApiError::internal(format!(
                "token refresh failed: HTTP {status}: {}",
                v.to_string().chars().take(300).collect::<String>()
            )));
        }
        let access = v
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("token refresh returned no access_token"))?
            .to_string();
        let new_refresh = v
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id_token = v
            .get("id_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        persist_tokens(
            &account.codex_home,
            &access,
            new_refresh.as_deref(),
            id_token.as_deref(),
        )
        .map_err(|e| ApiError::internal(format!("writing refreshed auth.json: {e:#}")))?;
        info!(account = %account.id, "access token refreshed");
        Ok(Tokens {
            account_id: current
                .account_id
                .clone()
                .or_else(|| account_id_from_jwt(&access)),
            access_token: access,
            refresh_token: new_refresh.or_else(|| current.refresh_token.clone()),
        })
    }
}

fn account_id_from_jwt(jwt: &str) -> Option<String> {
    parse_chatgpt_jwt_claims(jwt)
        .ok()
        .and_then(|c| c.chatgpt_account_id)
}

fn read_tokens(codex_home: &Path) -> Result<Tokens, ApiError> {
    let path = codex_home.join("auth.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| ApiError::internal(format!("reading {}: {e}", path.display())))?;
    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| ApiError::internal(format!("parsing {}: {e}", path.display())))?;
    let t = v.get("tokens").unwrap_or(&Value::Null);
    let field = |k: &str| {
        t.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let access_token = field("access_token").ok_or_else(|| {
        ApiError::internal(format!("{} has no tokens.access_token", path.display()))
    })?;
    Ok(Tokens {
        account_id: field("account_id").or_else(|| account_id_from_jwt(&access_token)),
        refresh_token: field("refresh_token"),
        access_token,
    })
}

/// Same fields Codex updates on refresh (`tokens.*`, `last_refresh`); atomic replace.
fn persist_tokens(
    codex_home: &Path,
    access_token: &str,
    refresh_token: Option<&str>,
    id_token: Option<&str>,
) -> anyhow::Result<()> {
    let path = codex_home.join("auth.json");
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let tokens = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("auth.json is not an object"))?
        .entry("tokens")
        .or_insert_with(|| json!({}));
    let t = tokens
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("auth.json tokens is not an object"))?;
    t.insert(
        "access_token".into(),
        Value::String(access_token.to_string()),
    );
    if let Some(r) = refresh_token {
        t.insert("refresh_token".into(), Value::String(r.to_string()));
    }
    if let Some(i) = id_token {
        t.insert("id_token".into(), Value::String(i.to_string()));
    }
    v["last_refresh"] = Value::String(Utc::now().to_rfc3339());
    let tmp = codex_home.join("auth.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Debug aid: with `CODEXS_DUMP_REQUESTS_DIR` set, the body as sent upstream
/// is written there as `raw-<ms>.json`.
fn dump_outgoing(body: &Bytes) {
    let Some(dir) = std::env::var_os("CODEXS_DUMP_REQUESTS_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if dir.as_os_str().is_empty() {
        return;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("raw-{ms}.json"));
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, body)) {
        warn!(path = %path.display(), "CODEXS_DUMP_REQUESTS_DIR: {e}");
    }
}

// ---- request shape ------------------------------------------------------

/// A body assembled by Codex: carries `client_metadata`, a Responses Lite
/// `additional_tools` item, or Codex's own base prompt as a developer message.
pub fn looks_like_codex_request(body: &Value) -> bool {
    if body.get("client_metadata").is_some_and(Value::is_object) {
        return true;
    }
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|it| {
                it.get("type").and_then(Value::as_str) == Some("additional_tools")
                    || (it.get("role").and_then(Value::as_str) == Some("developer")
                        && first_text(it).is_some_and(|t| t.starts_with("You are Codex")))
            })
        })
}

fn first_text(item: &Value) -> Option<&str> {
    match item.get("content") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(parts)) => parts
            .iter()
            .find_map(|p| p.get("text").and_then(Value::as_str)),
        _ => None,
    }
}

/// Headers that must not travel upstream: hop-by-hop, our own control headers,
/// downstream credentials, anything that leaks the downstream network.
fn drop_header(name: &HeaderName) -> bool {
    let n = name.as_str();
    matches!(
        n,
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "upgrade"
            | "accept-encoding"
            | "authorization"
            | "chatgpt-account-id"
            | "forwarded"
            | "x-real-ip"
            | "cf-connecting-ip"
            | "true-client-ip"
            | "x-codex-access-token"
            | "x-codex-id-token"
            | "x-codex-refresh-token"
            | "x-codex-account-id"
    ) || n.starts_with("x-asxs-")
        || n.starts_with("x-forwarded-")
}

/// Relay status, headers and the body stream untouched.
fn relay(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut hm = HeaderMap::new();
    for (k, v) in resp.headers() {
        if matches!(
            k.as_str(),
            "content-length" | "transfer-encoding" | "connection" | "content-encoding"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            hm.append(name, value);
        }
    }
    let mut out = Response::builder().status(status);
    if let Some(h) = out.headers_mut() {
        *h = hm;
    }
    out.body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_codex_shaped_bodies() {
        assert!(looks_like_codex_request(
            &json!({"client_metadata": {"session_id": "x"}})
        ));
        assert!(looks_like_codex_request(
            &json!({"input": [{"type": "additional_tools", "role": "developer", "tools": []}]})
        ));
        assert!(looks_like_codex_request(
            &json!({"input": [{"type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": "You are Codex, an agent"}]}]})
        ));
        assert!(!looks_like_codex_request(
            &json!({"input": "hi", "tools": [{"type": "function", "name": "f"}]})
        ));
        assert!(!looks_like_codex_request(
            &json!({"input": [{"role": "developer", "content": "Be terse."}]})
        ));
    }

    #[test]
    fn environment_context_is_localised() {
        let cfg = Arc::new(ProxyConfig::default());
        let tz = EgressTimezone::new(Some("Asia/Singapore"), reqwest::Client::new()).expect("tz");
        let fwd = RawForwarder::new(cfg, reqwest::Client::new(), tz.clone());
        let body = r#"{"input":[{"role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>C:\\x</cwd>\n  <current_date>2020-01-01</current_date>\n  <timezone>Asia/Shanghai</timezone>\n</environment_context>"}]}],"asxs":{"session_id":"s"}}"#;
        let parsed: Value = serde_json::from_str(body).expect("json");
        let (out, name) = fwd
            .prepare_body(&Bytes::from(body), &parsed)
            .expect("prepare");
        let out = String::from_utf8(out.to_vec()).expect("utf8");
        assert_eq!(name.as_deref(), Some("Asia/Singapore"));
        assert!(out.contains("<timezone>Asia/Singapore</timezone>"), "{out}");
        assert!(
            out.contains(&format!(
                "<current_date>{}</current_date>",
                tz.today().expect("today")
            )),
            "{out}"
        );
        assert!(!out.contains("asxs"), "{out}");
        assert!(out.contains("<cwd>C:\\\\x</cwd>"), "{out}");
    }

    #[test]
    fn header_filter() {
        for h in [
            "host",
            "authorization",
            "x-asxs-session",
            "x-forwarded-for",
            "accept-encoding",
            "chatgpt-account-id",
        ] {
            assert!(drop_header(&HeaderName::from_static(h)), "{h}");
        }
        for h in [
            "originator",
            "user-agent",
            "x-codex-installation-id",
            "session_id",
            "x-oai-attestation",
            "openai-beta",
        ] {
            assert!(!drop_header(&HeaderName::from_static(h)), "{h}");
        }
    }
}
