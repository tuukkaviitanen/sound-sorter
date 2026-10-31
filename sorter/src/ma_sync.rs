//! Optional, synchronous Music Assistant integration. No network work when disabled.
#[cfg(test)]
mod tests;
use crate::save_json;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::File,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct MaConfig {
    pub endpoint: String,
    pub token: String,
    pub music_provider: Option<String>,
    pub audiobook_provider: Option<String>,
    pub debounce: Duration,
    pub timeout: Duration,
    pub retry: Duration,
}
impl MaConfig {
    pub fn new(
        url: &str,
        token: String,
        music_provider: Option<String>,
        audiobook_provider: Option<String>,
    ) -> Result<Self> {
        let url = url.trim_end_matches('/');
        let uri: ureq::http::Uri = url.parse().context("Invalid MA_URL")?;
        ensure!(
            matches!(uri.scheme_str(), Some("http" | "https")) && uri.host().is_some(),
            "MA_URL must be an HTTP(S) URL"
        );
        ensure!(
            !uri.authority().is_some_and(|a| a.as_str().contains('@')) && !url.contains(['?', '#']),
            "MA_URL must not contain credentials, a query, or a fragment"
        );
        ensure!(
            !token.trim().is_empty() && !token.contains(['\r', '\n']),
            "MA_TOKEN must be a nonempty, single-line access token"
        );
        ensure!(
            music_provider.is_some() || audiobook_provider.is_some(),
            "Set MA_MUSIC_PROVIDER or MA_AUDIOBOOK_PROVIDER"
        );
        Ok(Self {
            endpoint: if url.ends_with("/api") {
                url.into()
            } else {
                format!("{url}/api")
            },
            token,
            music_provider,
            audiobook_provider,
            debounce: Duration::from_secs(30),
            timeout: Duration::from_secs(5),
            retry: Duration::from_secs(30),
        })
    }
    pub fn from_env() -> Result<Option<Self>> {
        let url = std::env::var("MA_URL").unwrap_or_default();
        if url.trim().is_empty() {
            return Ok(None);
        }
        fn provider(key: &str) -> Option<String> {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
        fn seconds(key: &str, default: u64) -> Result<Duration> {
            let value = std::env::var(key).map_or(Ok(default), |s| {
                s.parse::<u64>().with_context(|| format!("Invalid {key}"))
            })?;
            ensure!(
                value > 0 && value <= 86400,
                "{key} must be between 1 and 86400"
            );
            Ok(Duration::from_secs(value))
        }
        let token = if let Some(path) = std::env::var_os("MA_TOKEN_FILE").filter(|p| !p.is_empty())
        {
            std::fs::read_to_string(path)
                .context("Unable to read MA_TOKEN_FILE")?
                .trim()
                .into()
        } else {
            std::env::var("MA_TOKEN").context("Set MA_TOKEN or MA_TOKEN_FILE")?
        };
        let mut config = Self::new(
            &url,
            token,
            provider("MA_MUSIC_PROVIDER"),
            provider("MA_AUDIOBOOK_PROVIDER"),
        )?;
        config.debounce = seconds("MA_SYNC_DEBOUNCE_SECONDS", 30)?;
        config.timeout = seconds("MA_SYNC_TIMEOUT_SECONDS", 5)?;
        config.retry = seconds("MA_SYNC_RETRY_SECONDS", 30)?;
        Ok(Some(config))
    }
}

pub struct MusicAssistant {
    config: MaConfig,
    agent: ureq::Agent,
    state: PathBuf,
    providers: BTreeSet<String>,
    deadline: Instant,
    failures: u32,
}
impl MusicAssistant {
    pub fn new(config: MaConfig, state: &Path, now: Instant) -> Result<Self> {
        let providers: BTreeSet<String> = if state.exists() {
            serde_json::from_reader(File::open(state)?)?
        } else {
            BTreeSet::new()
        };
        let providers = providers
            .into_iter()
            .filter(|p| {
                config.music_provider.as_ref() == Some(p)
                    || config.audiobook_provider.as_ref() == Some(p)
            })
            .collect();
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(config.timeout))
            .max_redirects(0)
            .build()
            .into();
        let deadline = now + config.debounce;
        Ok(Self {
            config,
            agent,
            state: state.into(),
            providers,
            deadline,
            failures: 0,
        })
    }
    pub fn mark(&mut self, book: bool, now: Instant) -> Result<()> {
        let provider = if book {
            &self.config.audiobook_provider
        } else {
            &self.config.music_provider
        };
        if let Some(provider) = provider {
            let changed = self.providers.insert(provider.clone());
            self.deadline = now + self.config.debounce;
            self.failures = 0;
            if changed {
                save_json(&self.state, &self.providers)?;
            }
        }
        Ok(())
    }
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        (!self.providers.is_empty()).then(|| self.deadline.saturating_duration_since(now))
    }
    pub fn due(&self, now: Instant) -> bool {
        self.wait(now) == Some(Duration::ZERO)
    }
    pub fn defer(&mut self, now: Instant) {
        self.deadline = now + self.config.debounce;
    }
    fn request(&self) -> Result<()> {
        let mut response = self.agent.post(&self.config.endpoint)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .send_json(json!({"message_id": "upload-sorter-sync", "command": "music/sync", "args": {"providers": self.providers}}))
            // Never log request/response errors: they may contain server data or credentials.
            .map_err(|_| anyhow::anyhow!("Music Assistant HTTP request failed (check URL, token and connectivity)"))?;
        ensure!(
            response.status().is_success(),
            "Music Assistant returned a non-success HTTP status"
        );
        let body: Value = response
            .body_mut()
            .with_config()
            .limit(65536)
            .read_json()
            .map_err(|_| anyhow::anyhow!("Music Assistant returned an invalid API response"))?;
        // HTTP /api returns the command result directly, unlike the WebSocket
        // API's message envelope. Older servers return null; current servers
        // return background sync tasks. Never include response data in errors.
        let direct_result = body.is_null()
            || body.as_array().is_some_and(|tasks| {
                tasks.iter().all(|task| {
                    task.get("id").and_then(Value::as_str).is_some()
                        && task.get("status").and_then(Value::as_str).is_some()
                })
            });
        let enveloped_result = body.get("message_id").and_then(Value::as_str)
            == Some("upload-sorter-sync")
            && body.get("error_code").is_none()
            && body.get("error").is_none()
            && body.get("result").is_some();
        ensure!(
            direct_result || enveloped_result,
            "Music Assistant rejected the sync command or returned an unexpected response"
        );
        Ok(())
    }
    pub fn send(&mut self, now: Instant) -> Result<()> {
        // Save again on every attempt in case persisting the original mark failed.
        let result = save_json(&self.state, &self.providers)
            .and_then(|()| self.request())
            .and_then(|()| save_json(&self.state, &BTreeSet::<String>::new()));
        match result {
            Ok(()) => {
                eprintln!(
                    "Music Assistant sync requested for {} provider(s)",
                    self.providers.len()
                );
                self.providers.clear();
                self.failures = 0;
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                let delay = self
                    .config
                    .retry
                    .saturating_mul(1 << self.failures.saturating_sub(1).min(4))
                    .min(Duration::from_secs(300));
                self.deadline = now + delay;
                return Err(error);
            }
        }
        Ok(())
    }
}
