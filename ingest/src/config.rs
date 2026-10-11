//! Server configuration: structs, defaults, validation and the secrets that
//! are read from the environment and nowhere else.

use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

use ninja_monitor::config::{de_duration, de_opt_duration};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/ninja-ingest/config.toml";
pub const DISCORD_WEBHOOK_ENV: &str = "NINJA_INGEST_DISCORD_WEBHOOK";

/// Longest host, service or mount name accepted anywhere, in bytes.
pub const MAX_NAME_LEN: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub silence: SilenceConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default, rename = "agent")]
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub allow_non_loopback: bool,
    /// Browser origins allowed to call the read API. Empty disables CORS.
    pub cors_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 9091)),
            allow_non_loopback: false,
            cors_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub path: PathBuf,
    #[serde(deserialize_with = "de_duration")]
    pub retention: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub events_retention: Duration,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("/var/lib/ninja-ingest/ingest.db"),
            retention: Duration::from_secs(30 * 86_400),
            events_retention: Duration::from_secs(180 * 86_400),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordConfig {
    pub enabled: bool,
    pub startup_message: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            startup_message: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SilenceConfig {
    #[serde(deserialize_with = "de_duration")]
    pub after: Duration,
    #[serde(deserialize_with = "de_opt_duration")]
    pub repeat: Option<Duration>,
}

impl Default for SilenceConfig {
    fn default() -> Self {
        Self {
            after: Duration::from_secs(120),
            repeat: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    pub url_env: String,
    #[serde(deserialize_with = "de_duration")]
    pub interval: Duration,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url_env: "NINJA_INGEST_HEARTBEAT_URL".to_string(),
            interval: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Must equal `[host] name` on that agent.
    pub name: String,
    /// Environment variable holding the agent's bearer token.
    pub token_env: String,
}

/// Values read from the environment. Never logged, never serialised.
#[derive(Clone, Default)]
pub struct Secrets {
    /// `(agent name, token)` for every agent whose variable is set.
    pub tokens: Vec<(String, String)>,
    pub discord_webhook: Option<String>,
    pub heartbeat_url: Option<String>,
}

impl Secrets {
    /// Reads only the variables the config names. Empty values count as unset.
    pub fn from_env(config: &Config) -> Self {
        let read = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
            tokens: config
                .agents
                .iter()
                .filter_map(|a| read(&a.token_env).map(|t| (a.name.clone(), t)))
                .collect(),
            discord_webhook: config
                .discord
                .enabled
                .then(|| read(DISCORD_WEBHOOK_ENV))
                .flatten(),
            heartbeat_url: config
                .heartbeat
                .enabled
                .then(|| read(&config.heartbeat.url_env))
                .flatten(),
        }
    }

    pub fn token_for(&self, agent: &str) -> Option<&str> {
        self.tokens
            .iter()
            .find(|(name, _)| name == agent)
            .map(|(_, token)| token.as_str())
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |v: &Option<String>| if v.is_some() { "<set>" } else { "<unset>" };
        let agents: Vec<&str> = self.tokens.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("Secrets")
            .field("tokens_for", &agents)
            .field("discord_webhook", &set(&self.discord_webhook))
            .field("heartbeat_url", &set(&self.heartbeat_url))
            .finish()
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid config file {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Config> {
        let config: Config = toml::from_str(text)?;
        let errors = config.validate();
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        Ok(config)
    }

    /// Returns every problem found, so the owner can fix them all in one go.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if !self.server.allow_non_loopback && !self.server.bind.ip().is_loopback() {
            errors.push(format!(
                "server.bind {} is not a loopback address (set allow_non_loopback = true to override)",
                self.server.bind
            ));
        }
        for origin in &self.server.cors_origins {
            if !is_origin(origin) {
                errors.push(format!(
                    "server.cors_origins entry `{origin}` must be an origin like https://example.com, with no path"
                ));
            }
        }

        if self.storage.path.as_os_str().is_empty() {
            errors.push("storage.path must not be empty".to_string());
        }
        if self.storage.retention < Duration::from_secs(3600) {
            errors.push("storage.retention must be at least 1h".to_string());
        }
        if self.storage.events_retention < Duration::from_secs(3600) {
            errors.push("storage.events_retention must be at least 1h".to_string());
        }

        if self.silence.after.is_zero() {
            errors.push("silence.after must be greater than zero".to_string());
        }
        if self.silence.repeat.is_some_and(|r| r.is_zero()) {
            errors.push("silence.repeat must be greater than zero".to_string());
        }

        if self.heartbeat.enabled {
            if self.heartbeat.url_env.is_empty() {
                errors.push("heartbeat.url_env must not be empty".to_string());
            }
            if self.heartbeat.interval.is_zero() {
                errors.push("heartbeat.interval must be greater than zero".to_string());
            }
        }

        let mut names = HashSet::new();
        let mut envs = HashSet::new();
        for agent in &self.agents {
            if let Err(reason) = check_name(&agent.name) {
                errors.push(format!("agent name `{}` {reason}", agent.name));
            } else if !names.insert(agent.name.as_str()) {
                errors.push(format!("duplicate agent name `{}`", agent.name));
            }
            if agent.token_env.is_empty() {
                errors.push(format!(
                    "agent `{}`: token_env must not be empty",
                    agent.name
                ));
            } else if !envs.insert(agent.token_env.as_str()) {
                errors.push(format!(
                    "agent `{}`: token_env `{}` is used by another agent",
                    agent.name, agent.token_env
                ));
            }
        }

        errors
    }
}

/// The rule for host, service and mount names on the wire: 1 to 64 bytes of
/// printable ASCII, no leading or trailing spaces.
pub fn check_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("must not be empty");
    }
    if name.len() > MAX_NAME_LEN {
        return Err("is longer than 64 bytes");
    }
    if !name.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err("must be printable ASCII");
    }
    if name != name.trim() {
        return Err("must not start or end with a space");
    }
    Ok(())
}

fn is_origin(text: &str) -> bool {
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"));
    match rest {
        Some(host) => {
            !host.is_empty() && !host.contains('/') && !host.contains(char::is_whitespace)
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
        [server]
        bind = "127.0.0.1:9091"
        cors_origins = ["https://ninjagoldfinch.com", "https://ninjagoldfinch.nz"]

        [storage]
        path = "/var/lib/ninja-ingest/ingest.db"
        retention = "30d"
        events_retention = "180d"

        [discord]
        enabled = true

        [silence]
        after = "2m"
        repeat = "6h"

        [heartbeat]
        enabled = true
        interval = "60s"

        [[agent]]
        name = "vps"
        token_env = "NINJA_INGEST_TOKEN_VPS"

        [[agent]]
        name = "home"
        token_env = "NINJA_INGEST_TOKEN_HOME"
    "#;

    #[test]
    fn parses_full_config() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.silence.after, Duration::from_secs(120));
        assert_eq!(config.silence.repeat, Some(Duration::from_secs(6 * 3600)));
        assert_eq!(config.storage.retention, Duration::from_secs(30 * 86_400));
        assert_eq!(config.server.cors_origins.len(), 2);
    }

    #[test]
    fn defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.server.bind.port(), 9091);
        assert!(config.server.bind.ip().is_loopback());
        assert_eq!(config.silence.after, Duration::from_secs(120));
        assert!(config.silence.repeat.is_none());
        assert!(!config.discord.enabled);
        assert!(config.discord.startup_message);
        assert!(!config.heartbeat.enabled);
        assert!(config.agents.is_empty());
    }

    fn errors(text: &str) -> String {
        match Config::parse(text) {
            Ok(_) => panic!("expected an error for:\n{text}"),
            Err(err) => format!("{err:#}"),
        }
    }

    #[test]
    fn rejects_bad_values() {
        assert!(errors("[server]\nbind = \"0.0.0.0:9091\"").contains("loopback"));
        assert!(
            Config::parse("[server]\nbind = \"0.0.0.0:9091\"\nallow_non_loopback = true").is_ok()
        );
        assert!(errors("[server]\ncors_origins = [\"ninjagoldfinch.com\"]").contains("origin"));
        assert!(errors("[server]\ncors_origins = [\"https://a.com/\"]").contains("origin"));
        assert!(errors("[storage]\nretention = \"5m\"").contains("retention"));
        assert!(errors("[silence]\nafter = \"0s\"").contains("silence.after"));
        assert!(errors("[silence]\nrepeat = \"0s\"").contains("silence.repeat"));
        assert!(errors("[heartbeat]\nenabled = true\nurl_env = \"\"").contains("url_env"));
        assert!(errors("[bogus]\nx = 1").contains("bogus"));
        assert!(errors("[storage]\nretention = \"30 days\"").contains("duration"));
    }

    #[test]
    fn rejects_bad_agents() {
        let dup = "[[agent]]\nname = \"a\"\ntoken_env = \"A\"\n[[agent]]\nname = \"a\"\ntoken_env = \"B\"";
        assert!(errors(dup).contains("duplicate agent name"));
        let same_env = "[[agent]]\nname = \"a\"\ntoken_env = \"A\"\n[[agent]]\nname = \"b\"\ntoken_env = \"A\"";
        assert!(errors(same_env).contains("used by another agent"));
        assert!(errors("[[agent]]\nname = \"\"\ntoken_env = \"A\"").contains("must not be empty"));
        assert!(errors("[[agent]]\nname = \"a\"\ntoken_env = \"\"").contains("token_env"));
        assert!(
            errors("[[agent]]\nname = \"héllo\"\ntoken_env = \"A\"").contains("printable ASCII")
        );
        let long = format!(
            "[[agent]]\nname = \"{}\"\ntoken_env = \"A\"",
            "x".repeat(65)
        );
        assert!(errors(&long).contains("64 bytes"));
        assert!(errors("[[agent]]\nname = \"a \"\ntoken_env = \"A\"").contains("space"));
    }

    #[test]
    fn secrets_debug_hides_values() {
        let secrets = Secrets {
            tokens: vec![("vps".into(), "topsecret".into())],
            discord_webhook: Some("https://discord/secret".into()),
            heartbeat_url: None,
        };
        let text = format!("{secrets:?}");
        assert!(!text.contains("topsecret") && !text.contains("discord/"));
        assert!(text.contains("vps") && text.contains("<set>") && text.contains("<unset>"));
        assert_eq!(secrets.token_for("vps"), Some("topsecret"));
        assert_eq!(secrets.token_for("home"), None);
    }
}
