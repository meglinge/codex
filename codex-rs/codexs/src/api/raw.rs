//! Raw forward: a Codex-shaped Responses request — what Codex CLI (or anything
//! replaying its traffic) sends — goes upstream **as the client built it**:
//! its body, its headers, its telemetry. Only three things change:
//!
//! * credentials: `Authorization` / `chatgpt-account-id` become this instance's
//!   account (tokens refreshed here when expired);
//! * `<environment_context>`: `<timezone>` and `<current_date>` are rewritten
//!   to the egress IP's zone, so the request looks produced where it leaves;
//! * downstream-only headers (`x-asxs-*`, proxy/hop-by-hop, forwarded-for) are dropped;
//! * identifiers are mapped both ways (see `idmap`): the client's installation /
//!   session / thread / turn ids and prompt cache key become account-keyed
//!   stand-ins upstream, and upstream's `x-codex-turn-state` / `resp_…` ids reach
//!   the client only as sealed tokens that are opened again when echoed back.
//!
//! Nothing is bridged through the in-process Codex, so no second base prompt,
//! no second environment context. The upstream stream is relayed byte-for-byte.

use std::collections::HashMap;
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
use crate::idmap::IdMap;
use crate::tz::EgressTimezone;
use futures::StreamExt;

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
    uuid_re: regex::Regex,
    resp_re: regex::Regex,
    idmaps: std::sync::Mutex<HashMap<PathBuf, Arc<IdMap>>>,
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
            uuid_re: regex::Regex::new(
                r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
            )
            .expect("static regex"),
            resp_re: regex::Regex::new(r"resp_[0-9a-f]{16,}").expect("static regex"),
            idmaps: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn idmap_for(&self, account: &Account) -> Result<Arc<IdMap>, ApiError> {
        if let Ok(cache) = self.idmaps.lock()
            && let Some(m) = cache.get(&account.codex_home)
        {
            return Ok(Arc::clone(m));
        }
        let m = Arc::new(
            IdMap::load(&account.codex_home)
                .map_err(|e| ApiError::internal(format!("loading id-map key: {e:#}")))?,
        );
        if let Ok(mut cache) = self.idmaps.lock() {
            cache.insert(account.codex_home.clone(), Arc::clone(&m));
        }
        Ok(m)
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
        let idmap = self.idmap_for(&account)?;
        let mapping = Arc::new(self.build_mapping(headers, parsed, &idmap));
        let body = Bytes::from(self.map_request_body(&body, &mapping, &idmap)?);
        let headers = self.map_request_headers(headers, &mapping, &idmap);
        dump_outgoing(&body);
        let mut tokens = self.tokens_for(&account).await?;
        let mut resp = self.send(&headers, &tokens, &body, stream).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            warn!(account = %account.id, "upstream 401 on raw forward; refreshing token and retrying once");
            tokens = self.refresh(&account, &tokens).await?;
            resp = self.send(&headers, &tokens, &body, stream).await?;
        }
        info!(
            account = %account.id, model = %model, stream, tz = tz.as_deref().unwrap_or("-"),
            ids = mapping.pairs.len(), status = resp.status().as_u16(), "raw forward"
        );
        Ok(Some(self.relay(resp, stream, mapping, idmap)))
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

/// Which client headers carry identifiers that must be mapped.
const ID_HEADERS: &[&str] = &[
    "session_id",
    "x-codex-installation-id",
    "x-codex-window-id",
    "x-codex-parent-thread-id",
    "x-codex-turn-metadata",
];

/// Upstream headers that identify upstream's side of the request.
const UPSTREAM_ID_HEADERS: &[&str] = &["x-oai-request-id", "x-request-id", "cf-ray"];

/// Client id → upstream stand-in, for one request (both directions).
pub struct Mapping {
    pairs: Vec<(String, String)>,
}

impl Mapping {
    fn forward(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (from, to) in &self.pairs {
            out = out.replace(from, to);
        }
        out
    }

    fn reverse(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (from, to) in &self.pairs {
            out = out.replace(to, from);
        }
        out
    }
}

impl RawForwarder {
    /// Every UUID the client uses to identify itself: the id headers, the
    /// `client_metadata` object (including the JSON inside its
    /// `x-codex-turn-metadata` string) and the prompt cache key.
    fn build_mapping(&self, headers: &HeaderMap, parsed: &Value, idmap: &IdMap) -> Mapping {
        let mut haystack = String::new();
        for name in ID_HEADERS {
            if let Some(v) = headers.get(*name).and_then(|v| v.to_str().ok()) {
                haystack.push_str(v);
                haystack.push('\n');
            }
        }
        if let Some(cm) = parsed.get("client_metadata") {
            haystack.push_str(&cm.to_string());
            haystack.push('\n');
        }
        if let Some(k) = parsed.get("prompt_cache_key").and_then(Value::as_str) {
            haystack.push_str(k);
        }
        let mut seen = std::collections::HashSet::new();
        let mut pairs = Vec::new();
        for m in self.uuid_re.find_iter(&haystack) {
            let id = m.as_str().to_ascii_lowercase();
            if seen.insert(id.clone()) {
                let mapped = idmap.map_uuid(&id);
                pairs.push((id, mapped));
            }
        }
        Mapping { pairs }
    }

    /// Client ids → stand-ins; a `previous_response_id` we issued → upstream id.
    fn map_request_body(
        &self,
        body: &Bytes,
        mapping: &Mapping,
        idmap: &IdMap,
    ) -> Result<String, ApiError> {
        let text = std::str::from_utf8(body)
            .map_err(|_| ApiError::bad_request("request body is not UTF-8"))?;
        let mut text = mapping.forward(text);
        if let Ok(v) = serde_json::from_str::<Value>(&text)
            && let Some(prev) = v.get("previous_response_id").and_then(Value::as_str)
            && let Some(upstream) = open_response_id(idmap, prev)
        {
            text = text.replace(prev, &upstream);
        }
        Ok(text)
    }

    /// Same for headers; the turn-state token becomes upstream's blob again,
    /// and the client's attestation (bound to its own installation) is dropped.
    fn map_request_headers(
        &self,
        headers: &HeaderMap,
        mapping: &Mapping,
        idmap: &IdMap,
    ) -> HeaderMap {
        let mut out = HeaderMap::new();
        for (k, v) in headers {
            let name = k.as_str();
            if name == "x-oai-attestation" {
                continue;
            }
            if name == "x-codex-turn-state" {
                if let Some(blob) = v.to_str().ok().and_then(|t| idmap.open(t))
                    && let Ok(hv) = HeaderValue::from_bytes(&blob)
                {
                    out.append(k.clone(), hv);
                }
                continue;
            }
            if ID_HEADERS.contains(&name)
                && let Ok(s) = v.to_str()
                && let Ok(hv) = HeaderValue::from_str(&mapping.forward(s))
            {
                out.append(k.clone(), hv);
                continue;
            }
            out.append(k.clone(), v.clone());
        }
        out
    }

    /// Relay status, headers and the body stream, mapping identifiers back:
    /// upstream `resp_…` ids and the turn-state blob become sealed tokens, our
    /// stand-ins become the client's ids again, upstream request ids are dropped.
    fn relay(
        &self,
        resp: reqwest::Response,
        stream: bool,
        mapping: Arc<Mapping>,
        idmap: Arc<IdMap>,
    ) -> Response {
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut hm = HeaderMap::new();
        for (k, v) in resp.headers() {
            let name = k.as_str();
            if matches!(
                name,
                "content-length" | "transfer-encoding" | "connection" | "content-encoding"
            ) || UPSTREAM_ID_HEADERS.contains(&name)
            {
                continue;
            }
            let (Ok(hname), Ok(mut value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(v.as_bytes()),
            ) else {
                continue;
            };
            if name == "x-codex-turn-state" {
                match HeaderValue::from_str(&idmap.seal(v.as_bytes())) {
                    Ok(sealed) => value = sealed,
                    Err(_) => continue,
                }
            }
            hm.append(hname, value);
        }
        if !hm.contains_key(header::CONTENT_TYPE) {
            hm.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(if stream {
                    "text/event-stream; charset=utf-8"
                } else {
                    "application/json"
                }),
            );
        }
        let resp_re = self.resp_re.clone();
        let mut upstream = resp.bytes_stream();
        let body = async_stream::stream! {
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = upstream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.extend_from_slice(&bytes);
                        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                            let line: Vec<u8> = buf.drain(..=pos).collect();
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(rewrite_line(&line, &resp_re, &mapping, &idmap)));
                        }
                    }
                    Err(e) => {
                        yield Err(std::io::Error::other(e));
                        return;
                    }
                }
            }
            if !buf.is_empty() {
                yield Ok(Bytes::from(rewrite_line(&buf, &resp_re, &mapping, &idmap)));
            }
        };
        let mut out = Response::builder().status(status);
        if let Some(h) = out.headers_mut() {
            *h = hm;
        }
        out.body(Body::from_stream(body))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
    }
}

/// One line of the upstream body (SSE line or the whole JSON document).
fn rewrite_line(line: &[u8], resp_re: &regex::Regex, mapping: &Mapping, idmap: &IdMap) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(line) else {
        return line.to_vec();
    };
    let sealed = resp_re.replace_all(text, |c: &regex::Captures| seal_response_id(idmap, &c[0]));
    mapping.reverse(&sealed).into_bytes()
}

fn seal_response_id(idmap: &IdMap, upstream_id: &str) -> String {
    format!("resp_{}", idmap.seal(upstream_id.as_bytes()))
}

fn open_response_id(idmap: &IdMap, client_id: &str) -> Option<String> {
    let token = client_id.strip_prefix("resp_")?;
    let plain = idmap.open(token)?;
    String::from_utf8(plain)
        .ok()
        .filter(|s| s.starts_with("resp_"))
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

    fn forwarder() -> (RawForwarder, Arc<IdMap>) {
        let tz = EgressTimezone::new(Some("UTC"), reqwest::Client::new()).expect("tz");
        let fwd = RawForwarder::new(Arc::new(ProxyConfig::default()), reqwest::Client::new(), tz);
        (fwd, Arc::new(IdMap::from_key([9; 32])))
    }

    #[test]
    fn ids_are_mapped_both_ways() {
        let (fwd, idmap) = forwarder();
        let sid = "01a088c0-9905-7bc0-91d8-2609085bc21c";
        let inst = "0e5d2033-f9e2-467e-b0af-8635a87b8bbe";
        let body = json!({
            "prompt_cache_key": sid,
            "client_metadata": {"session_id": sid, "x-codex-installation-id": inst,
                "x-codex-turn-metadata": format!("{{\"installation_id\":\"{inst}\",\"session_id\":\"{sid}\"}}")},
            "input": [{"role": "user", "content": format!("my id is {sid}")}],
            "previous_response_id": seal_response_id(&idmap, "resp_abcdef0123456789abcdef"),
        });
        let mut headers = HeaderMap::new();
        headers.insert("session_id", HeaderValue::from_str(sid).unwrap());
        headers.insert(
            "x-codex-installation-id",
            HeaderValue::from_str(inst).unwrap(),
        );
        headers.insert("x-oai-attestation", HeaderValue::from_static("att"));
        headers.insert(
            "x-codex-turn-state",
            HeaderValue::from_str(&idmap.seal(b"upstream-blob")).unwrap(),
        );
        headers.insert(
            "x-codex-turn-state-foreign",
            HeaderValue::from_static("keep"),
        );
        let mapping = fwd.build_mapping(&headers, &body, &idmap);
        assert_eq!(mapping.pairs.len(), 2);
        let raw = Bytes::from(serde_json::to_string(&body).unwrap());
        let out = fwd.map_request_body(&raw, &mapping, &idmap).unwrap();
        assert!(!out.contains(sid) && !out.contains(inst), "{out}");
        assert!(out.contains(&idmap.map_uuid(sid)), "{out}");
        assert!(
            out.contains("resp_abcdef0123456789abcdef"),
            "previous_response_id opened: {out}"
        );
        let hm = fwd.map_request_headers(&headers, &mapping, &idmap);
        assert_eq!(
            hm.get("session_id").unwrap().to_str().unwrap(),
            idmap.map_uuid(sid)
        );
        assert_eq!(
            hm.get("x-codex-turn-state").unwrap().as_bytes(),
            b"upstream-blob"
        );
        assert!(hm.get("x-oai-attestation").is_none());
        // response direction
        let line = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_0bd94bc792496946016aa1f8\",\"prompt_cache_key\":\"{}\"}}}}\n",
            idmap.map_uuid(sid)
        );
        let back = String::from_utf8(rewrite_line(
            line.as_bytes(),
            &fwd.resp_re,
            &mapping,
            &idmap,
        ))
        .unwrap();
        assert!(!back.contains("resp_0bd94bc792496946016aa1f8"), "{back}");
        assert!(back.contains(sid), "prompt cache key mapped back: {back}");
        let token = back
            .split("\"id\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            open_response_id(&idmap, &token).as_deref(),
            Some("resp_0bd94bc792496946016aa1f8")
        );
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
