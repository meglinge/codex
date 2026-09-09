//! Client-supplied Codex credentials ("server mode").
//!
//! A request may carry its own ChatGPT OAuth material (access token JWT,
//! optional id token / refresh token / account id). Each distinct account gets
//! a private `CODEX_HOME` under `identity_root/<key>/` holding an `auth.json`
//! in Codex's own format, and its own in-process app-server. Codex then handles
//! the credentials exactly as it does for a `codex login` user: same headers,
//! same `chatgpt-account-id`, same refresh flow (when a refresh token is
//! present).

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use axum::http::HeaderMap;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_login::token_data::parse_jwt_expiration;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

#[derive(Debug, Clone)]
pub struct ClientCredentials {
    pub access_token: String,
    pub id_token: Option<String>,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Identity {
    /// Stable key (ChatGPT account id when available).
    pub key: String,
    pub codex_home: PathBuf,
    pub account_id: Option<String>,
}

pub fn looks_like_jwt(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty()) && s.starts_with("eyJ")
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Extract credentials from `asxs.auth` in the body, `x-codex-*` headers, or a
/// JWT bearer token. Returns `Ok(None)` when the request carries none.
pub fn extract_credentials(
    headers: &HeaderMap,
    body: &Value,
) -> anyhow::Result<Option<ClientCredentials>> {
    let auth = body.get("asxs").and_then(|a| a.get("auth"));
    let field = |k: &str| -> Option<String> {
        auth.and_then(|a| a.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut access_token = field("access_token")
        .or_else(|| header(headers, "x-codex-access-token").map(str::to_string));
    if access_token.is_none()
        && let Some(v) = header(headers, "authorization")
        && let Some(bearer) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        && looks_like_jwt(bearer.trim())
    {
        access_token = Some(bearer.trim().to_string());
    }
    let Some(access_token) = access_token else {
        return Ok(None);
    };
    anyhow::ensure!(
        looks_like_jwt(&access_token),
        "codex access token must be a JWT"
    );
    Ok(Some(ClientCredentials {
        access_token,
        id_token: field("id_token")
            .or_else(|| header(headers, "x-codex-id-token").map(str::to_string)),
        refresh_token: field("refresh_token")
            .or_else(|| header(headers, "x-codex-refresh-token").map(str::to_string)),
        account_id: field("account_id")
            .or_else(|| header(headers, "x-codex-account-id").map(str::to_string)),
    }))
}

fn sanitize_key(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.len() > 80 {
        out.truncate(80);
    }
    out
}

/// Resolve the identity for a set of credentials and make sure its
/// `CODEX_HOME` holds a matching `auth.json`. Returns `(identity, changed)`
/// where `changed` means the stored tokens were (re)written and any running
/// runtime for this identity must be restarted to pick them up.
pub fn materialize(
    root: &Path,
    config_template: Option<&Path>,
    creds: &ClientCredentials,
) -> anyhow::Result<(Identity, bool)> {
    let id_source = creds.id_token.as_deref().unwrap_or(&creds.access_token);
    let claims = parse_chatgpt_jwt_claims(id_source).ok();
    let access_claims = parse_chatgpt_jwt_claims(&creds.access_token).ok();
    let account_id = creds
        .account_id
        .clone()
        .or_else(|| claims.as_ref().and_then(|c| c.chatgpt_account_id.clone()))
        .or_else(|| {
            access_claims
                .as_ref()
                .and_then(|c| c.chatgpt_account_id.clone())
        });
    let key = match &account_id {
        Some(a) => sanitize_key(a),
        None => {
            let digest = Sha256::digest(creds.access_token.as_bytes());
            let mut s = String::from("tok_");
            for b in &digest[..12] {
                use std::fmt::Write;
                let _ = write!(s, "{b:02x}");
            }
            s
        }
    };
    let codex_home = root.join(&key);
    std::fs::create_dir_all(&codex_home)
        .with_context(|| format!("creating {}", codex_home.display()))?;
    let auth_path = codex_home.join("auth.json");

    let mut changed = false;
    let stored: Option<Value> = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let stored_access = stored
        .as_ref()
        .and_then(|v| v.get("tokens"))
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let should_write = match &stored_access {
        None => true,
        Some(existing) if existing == &creds.access_token => false,
        Some(existing) => {
            // Keep whichever token expires later: Codex may have refreshed the
            // stored one after the client obtained its copy.
            let stored_exp = parse_jwt_expiration(existing).ok().flatten();
            let presented_exp = parse_jwt_expiration(&creds.access_token).ok().flatten();
            match (stored_exp, presented_exp) {
                (Some(s), Some(p)) => p >= s,
                _ => true,
            }
        }
    };
    if should_write {
        let stored_tokens = stored.as_ref().and_then(|v| v.get("tokens"));
        let keep = |k: &str| {
            stored_tokens
                .and_then(|t| t.get(k))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let id_token = creds
            .id_token
            .clone()
            .or_else(|| keep("id_token"))
            .unwrap_or_else(|| creds.access_token.clone());
        let refresh_token = creds
            .refresh_token
            .clone()
            .or_else(|| keep("refresh_token"))
            .unwrap_or_default();
        let auth = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token,
                "access_token": creds.access_token,
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
            "last_refresh": chrono::Utc::now().to_rfc3339(),
        });
        let tmp = codex_home.join("auth.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&auth)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &auth_path)
            .with_context(|| format!("renaming into {}", auth_path.display()))?;
        changed = true;
    }
    if let Some(template) = config_template {
        let target = codex_home.join("config.toml");
        if !target.exists() && template.exists() {
            std::fs::copy(template, &target)
                .with_context(|| format!("copying config template to {}", target.display()))?;
        }
    }
    Ok((
        Identity {
            key,
            codex_home,
            account_id,
        },
        changed,
    ))
}

/// Read back the stored tokens for an identity (Codex may have refreshed them).
pub fn read_stored_auth(codex_home: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(codex_home.join("auth.json")).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let tokens = v.get("tokens")?;
    Some(json!({
        "access_token": tokens.get("access_token"),
        "id_token": tokens.get("id_token"),
        "refresh_token": tokens.get("refresh_token"),
        "account_id": tokens.get("account_id"),
        "last_refresh": v.get("last_refresh"),
        "access_token_expires_at": tokens
            .get("access_token")
            .and_then(Value::as_str)
            .and_then(|t| parse_jwt_expiration(t).ok().flatten())
            .map(|d| d.to_rfc3339()),
    }))
}
