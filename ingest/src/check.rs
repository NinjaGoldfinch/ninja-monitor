//! `--check`: confirms the storage path, tokens and secrets are usable on
//! this machine, as the user the daemon will run as.

use std::fs;

use crate::config::{Config, DISCORD_WEBHOOK_ENV, Secrets};
use crate::db::Db;
use crate::token::MIN_TOKEN_LEN;

/// One line per item, prefixed with `ok` or `WARN`. Never prints a secret.
pub fn report(config: &Config, secrets: &Secrets) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = |ok: bool, text: String| {
        lines.push(format!("{} {text}", if ok { "ok  " } else { "WARN" }));
    };

    let path = &config.storage.path;
    if path.exists() {
        match Db::open_existing(path).and_then(|db| db.schema_version_sync()) {
            Ok(version) => line(
                true,
                format!("database {} (schema version {version})", path.display()),
            ),
            Err(err) => line(false, format!("database {}: {err:#}", path.display())),
        }
    } else {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        match dir {
            Some(dir) if dir.is_dir() => {
                let probe = dir.join(".ninja-ingest-write-check");
                let writable = fs::write(&probe, b"").is_ok();
                let _ = fs::remove_file(&probe);
                line(
                    writable,
                    format!(
                        "database {} will be created in {}{}",
                        path.display(),
                        dir.display(),
                        if writable {
                            ""
                        } else {
                            ", which is not writable by this user"
                        }
                    ),
                );
            }
            Some(dir) => line(
                false,
                format!(
                    "database directory {} does not exist (systemd creates it from StateDirectory)",
                    dir.display()
                ),
            ),
            None => line(
                false,
                format!("database path {} has no directory", path.display()),
            ),
        }
    }

    if config.agents.is_empty() {
        line(
            false,
            "no [[agent]] configured; nothing can push samples yet".to_string(),
        );
    }
    for agent in &config.agents {
        match secrets.token_for(&agent.name) {
            Some(token) if token.len() >= MIN_TOKEN_LEN => line(
                true,
                format!("agent {}: {} set", agent.name, agent.token_env),
            ),
            Some(_) => line(
                false,
                format!(
                    "agent {}: {} is shorter than {MIN_TOKEN_LEN} characters; generate one with --new-token",
                    agent.name, agent.token_env
                ),
            ),
            None => line(
                false,
                format!(
                    "agent {}: {} not set in this environment (systemd loads it from the env file); this agent cannot authenticate",
                    agent.name, agent.token_env
                ),
            ),
        }
    }

    if config.discord.enabled {
        line(
            secrets.discord_webhook.is_some(),
            format!(
                "{DISCORD_WEBHOOK_ENV} set in this environment (systemd loads it from the env file)"
            ),
        );
    }
    if config.heartbeat.enabled {
        line(
            secrets.heartbeat_url.is_some(),
            format!(
                "{} set in this environment (systemd loads it from the env file)",
                config.heartbeat.url_env
            ),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(path: &std::path::Path) -> Config {
        Config::parse(&format!(
            r#"
            [storage]
            path = "{}"
            [discord]
            enabled = true
            [[agent]]
            name = "vps"
            token_env = "T_VPS"
            [[agent]]
            name = "home"
            token_env = "T_HOME"
            [[agent]]
            name = "short"
            token_env = "T_SHORT"
            "#,
            path.display()
        ))
        .unwrap()
    }

    #[test]
    fn reports_without_leaking_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ingest.db");
        let secrets = Secrets {
            tokens: vec![
                ("vps".into(), "a".repeat(64)),
                ("short".into(), "tooshort".into()),
            ],
            discord_webhook: Some("https://discord.example/hook".into()),
            heartbeat_url: None,
        };

        // Fresh install: directory exists, file does not.
        let lines = report(&config(&path), &secrets);
        assert!(
            lines[0].starts_with("ok") && lines[0].contains("will be created"),
            "{lines:#?}"
        );
        assert!(!path.exists(), "--check must not create the database");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("ok") && l.contains("agent vps"))
        );
        assert!(
            lines.iter().any(|l| l.starts_with("WARN")
                && l.contains("agent home")
                && l.contains("not set"))
        );
        assert!(
            lines.iter().any(|l| l.starts_with("WARN")
                && l.contains("agent short")
                && l.contains("shorter"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("ok") && l.contains(DISCORD_WEBHOOK_ENV))
        );
        assert!(
            lines
                .iter()
                .all(|l| !l.contains("aaaa") && !l.contains("tooshort") && !l.contains("http"))
        );

        // After the daemon has run once.
        drop(Db::open(&path).unwrap());
        let lines = report(&config(&path), &secrets);
        assert!(
            lines[0].starts_with("ok") && lines[0].contains("schema version"),
            "{lines:#?}"
        );

        // Missing directory.
        let lines = report(&config(&dir.path().join("nope/ingest.db")), &secrets);
        assert!(
            lines[0].starts_with("WARN") && lines[0].contains("does not exist"),
            "{lines:#?}"
        );

        let lines = report(&Config::parse("").unwrap(), &Secrets::default());
        assert!(lines.iter().any(|l| l.contains("no [[agent]]")));
    }
}
