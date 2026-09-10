//! Time zone of the egress IP (the account's proxy exit), used to localise the
//! `<environment_context>` of raw-forwarded requests so the request looks like
//! it was produced where the traffic leaves. Detected through the same outbound
//! proxy the upstream traffic uses; `codex.timezone` overrides detection.

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use chrono::Utc;
use chrono_tz::Tz;
use tracing::info;
use tracing::warn;

const REFRESH_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);

pub struct EgressTimezone {
    fixed: Option<Tz>,
    detected: RwLock<Option<Tz>>,
    client: reqwest::Client,
}

impl EgressTimezone {
    pub fn new(override_name: Option<&str>, client: reqwest::Client) -> anyhow::Result<Arc<Self>> {
        let fixed = match override_name.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => Some(name.parse::<Tz>().map_err(|e| {
                anyhow::anyhow!("codex.timezone {name:?} is not an IANA zone: {e}")
            })?),
            None => None,
        };
        Ok(Arc::new(Self {
            fixed,
            detected: RwLock::new(None),
            client,
        }))
    }

    /// The zone to stamp into requests, if known.
    pub fn current(&self) -> Option<Tz> {
        self.fixed
            .or_else(|| self.detected.read().ok().and_then(|g| *g))
    }

    /// Today's date in that zone (`YYYY-MM-DD`).
    pub fn today(&self) -> Option<String> {
        self.current()
            .map(|tz| Utc::now().with_timezone(&tz).format("%Y-%m-%d").to_string())
    }

    /// Detect now and keep re-detecting in the background (no-op with an override).
    pub fn spawn_refresh(self: &Arc<Self>) {
        if self.fixed.is_some() {
            info!(timezone = %self.fixed.unwrap_or(Tz::UTC), "egress timezone fixed by config");
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let wait = match detect(&me.client).await {
                    Ok((name, tz)) => {
                        let changed = me.current() != Some(tz);
                        if let Ok(mut g) = me.detected.write() {
                            *g = Some(tz);
                        }
                        if changed {
                            info!(timezone = %name, "egress timezone detected");
                        }
                        REFRESH_EVERY
                    }
                    Err(e) => {
                        warn!("egress timezone detection failed: {e:#}");
                        RETRY_AFTER_FAILURE
                    }
                };
                tokio::time::sleep(wait).await;
            }
        });
    }
}

async fn detect(client: &reqwest::Client) -> anyhow::Result<(String, Tz)> {
    let mut errors = Vec::new();
    for (url, field) in [
        ("https://ipapi.co/json/", "timezone"),
        ("https://ipinfo.io/json", "timezone"),
        ("http://ip-api.com/json/?fields=timezone", "timezone"),
    ] {
        match fetch_field(client, url, field).await {
            Ok(name) => match name.parse::<Tz>() {
                Ok(tz) => return Ok((name, tz)),
                Err(e) => errors.push(format!("{url}: {name:?} is not an IANA zone ({e})")),
            },
            Err(e) => errors.push(format!("{url}: {e:#}")),
        }
    }
    anyhow::bail!("{}", errors.join("; "))
}

async fn fetch_field(client: &reqwest::Client, url: &str, field: &str) -> anyhow::Result<String> {
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, "curl/8.5.0")
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        anyhow::bail!(
            "HTTP {status}: {}",
            body.to_string().chars().take(200).collect::<String>()
        );
    }
    body.get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no {field} in response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_is_parsed() {
        let tz = EgressTimezone::new(Some("Asia/Singapore"), reqwest::Client::new()).expect("tz");
        assert_eq!(tz.current(), Some(chrono_tz::Asia::Singapore));
        assert_eq!(tz.today().map(|d| d.len()), Some(10));
        assert!(EgressTimezone::new(Some("Mars/Olympus"), reqwest::Client::new()).is_err());
    }
}
