//! Configuration file structs, defaults, duration parsing and validation.

use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Deserializer};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/ninja-monitor/config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub sampling: SamplingConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub host: HostConfig,
    #[serde(default, rename = "service")]
    pub services: Vec<ServiceConfig>,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default, rename = "alert")]
    pub alerts: Vec<AlertRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub allow_non_loopback: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 9090)),
            allow_non_loopback: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SamplingConfig {
    #[serde(deserialize_with = "de_duration")]
    pub interval: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub disk_interval: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub history: Duration,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(15),
            disk_interval: Duration::from_secs(10 * 60),
            history: Duration::from_secs(24 * 60 * 60),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PathsConfig {
    pub cgroup_root: PathBuf,
    pub proc_root: PathBuf,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            proc_root: PathBuf::from("/proc"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    pub name: String,
    pub mounts: Vec<PathBuf>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            name: "vps".to_string(),
            mounts: vec![PathBuf::from("/")],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub unit: String,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Cgroup directory relative to `cgroup_root`. Defaults to `system.slice/{unit}`.
    #[serde(default)]
    pub cgroup: Option<String>,
}

impl ServiceConfig {
    pub fn cgroup_rel(&self) -> PathBuf {
        match &self.cgroup {
            Some(path) => PathBuf::from(path),
            None => Path::new("system.slice").join(&self.unit),
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
            url_env: "NINJA_MONITOR_HEARTBEAT_URL".to_string(),
            interval: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRule {
    pub name: String,
    pub scope: Scope,
    pub metric: Metric,
    #[serde(default)]
    pub op: Option<Op>,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub mount: Option<PathBuf>,
    #[serde(default, rename = "for", deserialize_with = "de_opt_duration")]
    pub for_duration: Option<Duration>,
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub repeat: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum Scope {
    Host,
    Service(String),
}

impl TryFrom<String> for Scope {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value == "host" {
            return Ok(Scope::Host);
        }
        match value.strip_prefix("service:") {
            Some(name) if !name.is_empty() => Ok(Scope::Service(name.to_string())),
            _ => Err(format!(
                "invalid scope `{value}`, expected `host` or `service:<name>`"
            )),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Host => f.write_str("host"),
            Scope::Service(name) => write!(f, "service:{name}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    CpuPercent,
    Load1,
    MemAvailablePercent,
    MemAvailableBytes,
    SwapUsedPercent,
    DiskUsedPercent,
    MemAnonBytes,
    MemCurrentBytes,
    Pids,
    DiskBytes,
    Down,
    OomKill,
}

impl Metric {
    fn allowed_for_host(self) -> bool {
        matches!(
            self,
            Metric::CpuPercent
                | Metric::Load1
                | Metric::MemAvailablePercent
                | Metric::MemAvailableBytes
                | Metric::SwapUsedPercent
                | Metric::DiskUsedPercent
        )
    }

    fn allowed_for_service(self) -> bool {
        matches!(
            self,
            Metric::CpuPercent
                | Metric::MemAnonBytes
                | Metric::MemCurrentBytes
                | Metric::Pids
                | Metric::DiskBytes
                | Metric::Down
                | Metric::OomKill
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            Metric::CpuPercent => "cpu_percent",
            Metric::Load1 => "load1",
            Metric::MemAvailablePercent => "mem_available_percent",
            Metric::MemAvailableBytes => "mem_available_bytes",
            Metric::SwapUsedPercent => "swap_used_percent",
            Metric::DiskUsedPercent => "disk_used_percent",
            Metric::MemAnonBytes => "mem_anon_bytes",
            Metric::MemCurrentBytes => "mem_current_bytes",
            Metric::Pids => "pids",
            Metric::DiskBytes => "disk_bytes",
            Metric::Down => "down",
            Metric::OomKill => "oom_kill",
        }
    }

    /// `down` and `oom_kill` take no operator or threshold.
    pub fn is_comparison(self) -> bool {
        !matches!(self, Metric::Down | Metric::OomKill)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Op {
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
}

impl Op {
    pub fn symbol(self) -> &'static str {
        match self {
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Lt => "<",
            Op::Le => "<=",
        }
    }

    pub fn holds(self, value: f64, threshold: f64) -> bool {
        match self {
            Op::Gt => value > threshold,
            Op::Ge => value >= threshold,
            Op::Lt => value < threshold,
            Op::Le => value <= threshold,
        }
    }
}

pub const DISCORD_WEBHOOK_ENV: &str = "NINJA_MONITOR_DISCORD_WEBHOOK";

/// URLs that must never appear in config, logs or API responses.
#[derive(Clone, Default)]
pub struct Secrets {
    pub discord_webhook: Option<String>,
    pub heartbeat_url: Option<String>,
}

impl Secrets {
    /// Reads only the variables the config enables. Empty values count as unset.
    pub fn from_env(config: &Config) -> Self {
        let read = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
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
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |v: &Option<String>| if v.is_some() { "<set>" } else { "<unset>" };
        f.debug_struct("Secrets")
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

        let sampling = &self.sampling;
        if sampling.interval.is_zero() {
            errors.push("sampling.interval must be greater than zero".to_string());
        }
        if sampling.disk_interval.is_zero() {
            errors.push("sampling.disk_interval must be greater than zero".to_string());
        }
        if sampling.history < sampling.interval {
            errors.push("sampling.history must be at least sampling.interval".to_string());
        }

        if self.host.name.trim().is_empty() {
            errors.push("host.name must not be empty".to_string());
        }

        let mut service_names = HashSet::new();
        for service in &self.services {
            if service.name.trim().is_empty() {
                errors.push("service.name must not be empty".to_string());
            } else if !service_names.insert(service.name.as_str()) {
                errors.push(format!("duplicate service name `{}`", service.name));
            }
            if service.unit.is_empty() || service.unit.contains('/') {
                errors.push(format!(
                    "service `{}`: unit `{}` must be a plain unit name",
                    service.name, service.unit
                ));
            }
            if let Some(cgroup) = &service.cgroup
                && !is_plain_relative(Path::new(cgroup))
            {
                errors.push(format!(
                    "service `{}`: cgroup `{cgroup}` must be a relative path without `..`",
                    service.name
                ));
            }
        }

        if self.heartbeat.enabled {
            if self.heartbeat.url_env.is_empty() {
                errors.push("heartbeat.url_env must not be empty".to_string());
            }
            if self.heartbeat.interval.is_zero() {
                errors.push("heartbeat.interval must be greater than zero".to_string());
            }
        }

        let mut rule_names = HashSet::new();
        for rule in &self.alerts {
            if !rule_names.insert(rule.name.as_str()) {
                errors.push(format!("duplicate alert name `{}`", rule.name));
            }
            self.validate_rule(rule, &service_names, &mut errors);
        }

        errors
    }

    fn validate_rule(&self, rule: &AlertRule, services: &HashSet<&str>, errors: &mut Vec<String>) {
        let mut err = |msg: String| errors.push(format!("alert `{}`: {msg}", rule.name));

        match &rule.scope {
            Scope::Host if !rule.metric.allowed_for_host() => {
                err(format!(
                    "metric {:?} is not available for host scope",
                    rule.metric
                ));
            }
            Scope::Service(name) => {
                if !services.contains(name.as_str()) {
                    err(format!("unknown service `{name}`"));
                }
                if !rule.metric.allowed_for_service() {
                    err(format!(
                        "metric {:?} is not available for service scope",
                        rule.metric
                    ));
                }
            }
            Scope::Host => {}
        }

        if rule.metric.is_comparison() {
            if rule.op.is_none() {
                err("`op` is required".to_string());
            }
            match rule.threshold {
                None => err("`threshold` is required".to_string()),
                Some(t) if !t.is_finite() => err("`threshold` must be a finite number".to_string()),
                Some(_) => {}
            }
        } else if rule.op.is_some() || rule.threshold.is_some() {
            err("`op` and `threshold` are not used by this metric".to_string());
        }

        match (&rule.mount, rule.metric) {
            (None, Metric::DiskUsedPercent) => err("`mount` is required".to_string()),
            (Some(mount), Metric::DiskUsedPercent) => {
                if !self.host.mounts.contains(mount) {
                    err(format!(
                        "mount {} is not listed in host.mounts",
                        mount.display()
                    ));
                }
            }
            (Some(_), _) => err("`mount` is only used by disk_used_percent".to_string()),
            (None, _) => {}
        }

        if rule.metric == Metric::OomKill {
            if rule.for_duration.is_some() || rule.repeat.is_some() {
                err("oom_kill is an event rule and takes no `for` or `repeat`".to_string());
            }
        } else if rule.repeat.is_some_and(|r| r.is_zero()) {
            err("`repeat` must be greater than zero".to_string());
        }
    }
}

fn is_plain_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Parses durations such as `500ms`, `15s`, `5m`, `6h` or `1d`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("duration `{text}` is missing a unit (ms, s, m, h or d)"))?;
    let (number, unit) = text.split_at(split);
    let value: u64 = number
        .parse()
        .map_err(|_| format!("duration `{text}` must start with a whole number"))?;
    let millis_per_unit = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(format!("duration `{text}` has unknown unit `{unit}`")),
    };
    value
        .checked_mul(millis_per_unit)
        .map(Duration::from_millis)
        .ok_or_else(|| format!("duration `{text}` is too large"))
}

fn de_duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_duration(&text).map_err(serde::de::Error::custom)
}

fn de_opt_duration<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    de_duration(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
        [server]
        bind = "127.0.0.1:9090"

        [sampling]
        interval = "15s"
        disk_interval = "10m"
        history = "24h"

        [host]
        name = "vps"
        mounts = ["/"]

        [[service]]
        name = "riot-proxy"
        unit = "riot-proxy.service"
        paths = ["/var/lib/riot-proxy"]

        [[service]]
        name = "cloudflared"
        unit = "cloudflared.service"
        cgroup = "system.slice/cloudflared.service"

        [discord]
        enabled = true

        [[alert]]
        name = "mem"
        scope = "service:riot-proxy"
        metric = "mem_anon_bytes"
        op = ">"
        threshold = 2000000000
        for = "5m"

        [[alert]]
        name = "disk"
        scope = "host"
        metric = "disk_used_percent"
        mount = "/"
        op = ">="
        threshold = 85.5
        repeat = "6h"

        [[alert]]
        name = "down"
        scope = "service:riot-proxy"
        metric = "down"
        for = "1m"

        [[alert]]
        name = "oom"
        scope = "service:riot-proxy"
        metric = "oom_kill"
    "#;

    fn errors_for(text: &str) -> String {
        format!("{:#}", Config::parse(text).unwrap_err())
    }

    #[test]
    fn parses_full_config() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(config.sampling.interval, Duration::from_secs(15));
        assert_eq!(config.services.len(), 2);
        assert_eq!(
            config.services[0].cgroup_rel(),
            PathBuf::from("system.slice/riot-proxy.service")
        );
        assert_eq!(config.alerts.len(), 4);
        assert_eq!(config.alerts[0].op, Some(Op::Gt));
        assert_eq!(config.alerts[0].threshold, Some(2e9));
        assert_eq!(
            config.alerts[0].for_duration,
            Some(Duration::from_secs(300))
        );
        assert_eq!(config.alerts[1].repeat, Some(Duration::from_secs(6 * 3600)));
        assert_eq!(
            config.alerts[0].scope,
            Scope::Service("riot-proxy".to_string())
        );
        assert!(config.discord.enabled);
        assert!(config.discord.startup_message);
    }

    #[test]
    fn empty_config_uses_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.server.bind.to_string(), "127.0.0.1:9090");
        assert_eq!(config.paths.proc_root, PathBuf::from("/proc"));
        assert_eq!(config.paths.cgroup_root, PathBuf::from("/sys/fs/cgroup"));
        assert_eq!(config.sampling.disk_interval, Duration::from_secs(600));
        assert_eq!(config.host.mounts, vec![PathBuf::from("/")]);
        assert!(config.services.is_empty());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("15s"), Ok(Duration::from_secs(15)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("6h"), Ok(Duration::from_secs(21600)));
        assert_eq!(parse_duration("1d"), Ok(Duration::from_secs(86400)));
        assert!(parse_duration("15").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("1.5s").is_err());
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("-1s").is_err());
        assert!(parse_duration("99999999999999999999d").is_err());
    }

    #[test]
    fn rejects_bad_duration_in_file() {
        assert!(errors_for("[sampling]\ninterval = \"fast\"").contains("fast"));
    }

    #[test]
    fn rejects_non_loopback_bind() {
        let msg = errors_for("[server]\nbind = \"0.0.0.0:9090\"");
        assert!(msg.contains("not a loopback"), "{msg}");
        Config::parse("[server]\nbind = \"0.0.0.0:9090\"\nallow_non_loopback = true").unwrap();
        Config::parse("[server]\nbind = \"[::1]:9090\"").unwrap();
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(errors_for("[sampling]\nintervall = \"15s\"").contains("intervall"));
        assert!(errors_for("[bogus]\nx = 1").contains("bogus"));
    }

    #[test]
    fn rejects_zero_interval_and_short_history() {
        let msg = errors_for("[sampling]\ninterval = \"0s\"\nhistory = \"10s\"\n");
        assert!(msg.contains("interval must be greater than zero"), "{msg}");
        let msg = errors_for("[sampling]\ninterval = \"1m\"\nhistory = \"30s\"\n");
        assert!(msg.contains("history must be at least"), "{msg}");
    }

    #[test]
    fn rejects_bad_services() {
        let msg = errors_for(
            r#"
            [[service]]
            name = "a"
            unit = "a.service"
            [[service]]
            name = "a"
            unit = "x/y"
            cgroup = "../etc"
            "#,
        );
        assert!(msg.contains("duplicate service name `a`"), "{msg}");
        assert!(msg.contains("plain unit name"), "{msg}");
        assert!(msg.contains("without `..`"), "{msg}");
        let msg = errors_for("[[service]]\nname = \"a\"\nunit = \"a.service\"\ncgroup = \"/abs\"");
        assert!(msg.contains("without `..`"), "{msg}");
    }

    #[test]
    fn rejects_unknown_metric_and_scope() {
        let msg = errors_for(
            "[[alert]]\nname = \"x\"\nscope = \"host\"\nmetric = \"bogus\"\nop = \">\"\nthreshold = 1",
        );
        assert!(msg.contains("bogus"), "{msg}");
        let msg = errors_for(
            "[[alert]]\nname = \"x\"\nscope = \"galaxy\"\nmetric = \"cpu_percent\"\nop = \">\"\nthreshold = 1",
        );
        assert!(msg.contains("invalid scope"), "{msg}");
        let msg = errors_for(
            "[[alert]]\nname = \"x\"\nscope = \"service:nope\"\nmetric = \"cpu_percent\"\nop = \">\"\nthreshold = 1",
        );
        assert!(msg.contains("unknown service `nope`"), "{msg}");
    }

    #[test]
    fn rejects_metric_in_wrong_scope() {
        let msg = errors_for("[[alert]]\nname = \"x\"\nscope = \"host\"\nmetric = \"down\"");
        assert!(msg.contains("not available for host scope"), "{msg}");
        let msg = errors_for(
            "[[service]]\nname = \"s\"\nunit = \"s.service\"\n[[alert]]\nname = \"x\"\nscope = \"service:s\"\nmetric = \"load1\"\nop = \">\"\nthreshold = 1",
        );
        assert!(msg.contains("not available for service scope"), "{msg}");
    }

    #[test]
    fn rejects_bad_rule_shapes() {
        let msg = errors_for("[[alert]]\nname = \"x\"\nscope = \"host\"\nmetric = \"load1\"");
        assert!(msg.contains("`op` is required"), "{msg}");
        assert!(msg.contains("`threshold` is required"), "{msg}");

        let msg = errors_for(
            "[[alert]]\nname = \"x\"\nscope = \"host\"\nmetric = \"load1\"\nop = \"!=\"\nthreshold = 1",
        );
        assert!(msg.contains("!="), "{msg}");

        let msg = errors_for(
            "[[alert]]\nname = \"x\"\nscope = \"host\"\nmetric = \"disk_used_percent\"\nop = \">\"\nthreshold = 1",
        );
        assert!(msg.contains("`mount` is required"), "{msg}");

        let msg = errors_for(
            "[[alert]]\nname = \"x\"\nscope = \"host\"\nmetric = \"disk_used_percent\"\nmount = \"/data\"\nop = \">\"\nthreshold = 1",
        );
        assert!(msg.contains("not listed in host.mounts"), "{msg}");

        let msg = errors_for(
            "[[alert]]\nname = \"x\"\nscope = \"host\"\nmetric = \"load1\"\nmount = \"/\"\nop = \">\"\nthreshold = 1",
        );
        assert!(msg.contains("only used by disk_used_percent"), "{msg}");

        let msg = errors_for(
            "[[service]]\nname = \"s\"\nunit = \"s.service\"\n[[alert]]\nname = \"x\"\nscope = \"service:s\"\nmetric = \"oom_kill\"\nfor = \"1m\"\nop = \">\"",
        );
        assert!(msg.contains("takes no `for`"), "{msg}");
        assert!(msg.contains("not used by this metric"), "{msg}");
    }

    #[test]
    fn rejects_duplicate_alert_names() {
        let rule = "[[alert]]\nname = \"x\"\nscope = \"host\"\nmetric = \"load1\"\nop = \">\"\nthreshold = 1\n";
        let msg = errors_for(&format!("{rule}{rule}"));
        assert!(msg.contains("duplicate alert name `x`"), "{msg}");
    }

    #[test]
    fn operators() {
        assert!(Op::Gt.holds(2.0, 1.0) && !Op::Gt.holds(1.0, 1.0));
        assert!(Op::Ge.holds(1.0, 1.0) && !Op::Ge.holds(0.5, 1.0));
        assert!(Op::Lt.holds(0.5, 1.0) && !Op::Lt.holds(1.0, 1.0));
        assert!(Op::Le.holds(1.0, 1.0) && !Op::Le.holds(1.5, 1.0));
    }

    #[test]
    fn secrets_debug_is_redacted() {
        let secrets = Secrets {
            discord_webhook: Some("https://discord.com/api/webhooks/1/secret".into()),
            heartbeat_url: None,
        };
        let text = format!("{secrets:?}");
        assert!(!text.contains("discord.com") && !text.contains("webhooks"));
        assert!(text.contains("<set>"));
        assert!(text.contains("<unset>"));
    }

    #[test]
    fn shipped_configs_are_valid() {
        let example = Config::parse(include_str!("../deploy/config.example.toml")).unwrap();
        assert_eq!(example.services.len(), 3);
        assert_eq!(example.alerts.len(), 5);
        assert!(example.discord.enabled && !example.heartbeat.enabled);
        Config::parse(include_str!("../dev/config.toml")).unwrap();
    }

    #[test]
    fn missing_file_is_a_clear_error() {
        let msg = format!(
            "{:#}",
            Config::load(Path::new("/definitely/not/here.toml")).unwrap_err()
        );
        assert!(msg.contains("cannot read config file"), "{msg}");
    }
}
