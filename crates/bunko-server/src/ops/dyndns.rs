//! Background dynamic-DNS updater (0.5.2 `dyndns/service.py`).

use bunko_core::config::DynDnsConfig;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct State {
    last_update: Option<String>,
    last_ip: Option<String>,
    last_error: Option<String>,
}

#[derive(Clone)]
pub struct DynDnsService {
    inner: Arc<Inner>,
}

struct Inner {
    config: Mutex<DynDnsConfig>,
    state: Mutex<State>,
    running: Mutex<Option<CancellationToken>>,
    client: reqwest::Client,
}

impl DynDnsService {
    pub fn new(config: DynDnsConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                config: Mutex::new(config),
                state: Mutex::new(State::default()),
                running: Mutex::new(None),
                client,
            }),
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.running.lock().is_some()
    }

    /// Update now, then every `interval` seconds until stopped.
    pub fn start(&self) {
        let mut running = self.inner.running.lock();
        if running.is_some() {
            return;
        }
        let token = CancellationToken::new();
        *running = Some(token.clone());
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                me.update_now().await;
                let interval = me.inner.config.lock().interval.max(30);
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(interval as u64)) => {}
                }
            }
        });
    }

    pub fn stop(&self) {
        if let Some(t) = self.inner.running.lock().take() {
            t.cancel();
        }
    }

    /// Swap the configuration; restart if it was running and is still enabled.
    pub fn configure(&self, config: DynDnsConfig) {
        let was_running = self.is_running();
        self.stop();
        let enabled = config.enabled;
        *self.inner.config.lock() = config;
        if was_running && enabled {
            self.start();
        }
    }

    pub fn status(&self) -> Value {
        let c = self.inner.config.lock().clone();
        let s = self.inner.state.lock();
        json!({
            "enabled": c.enabled,
            "running": self.is_running(),
            "provider": c.provider,
            "domain": c.domain,
            "last_update": s.last_update,
            "last_ip": s.last_ip,
            "last_error": s.last_error,
        })
    }

    pub async fn update_now(&self) -> Value {
        match self.do_update().await {
            Ok((ip, body)) => {
                let mut s = self.inner.state.lock();
                s.last_update = Some(utc_stamp());
                s.last_error = None;
                json!({"success": true, "ip": ip, "response": body})
            }
            Err(e) => {
                self.inner.state.lock().last_error = Some(e.clone());
                json!({"success": false, "error": e})
            }
        }
    }

    async fn get_text(&self, url: &str) -> Result<String, String> {
        let resp = self
            .inner
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "HTTP Error {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            ));
        }
        Ok(body.trim().to_string())
    }

    async fn do_update(&self) -> Result<(String, String), String> {
        let ip = self.get_text("https://api.ipify.org").await?;
        self.inner.state.lock().last_ip = Some(ip.clone());
        let c = self.inner.config.lock().clone();
        let body = if c.provider == "duckdns" {
            let domain = c.domain.strip_suffix(".duckdns.org").unwrap_or(&c.domain);
            let body = self
                .get_text(&format!(
                    "https://www.duckdns.org/update?domains={domain}&token={}&ip={ip}",
                    c.token
                ))
                .await?;
            if body != "OK" {
                return Err(format!("DuckDNS update failed: {body}"));
            }
            body
        } else {
            if c.update_url.is_empty() {
                return Err("No update_url configured for generic provider".into());
            }
            let url = c
                .update_url
                .replace("{ip}", &ip)
                .replace("{domain}", &c.domain)
                .replace("{token}", &c.token);
            self.get_text(&url).await?
        };
        Ok((ip, body))
    }
}

/// `%Y-%m-%dT%H:%M:%SZ` in UTC.
pub fn utc_stamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}
