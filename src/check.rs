//! `--check`: confirms the paths a config names are usable on this machine.

use std::fs;

use crate::config::{Config, DISCORD_WEBHOOK_ENV, Secrets};
use crate::sampler::disk::mount_usage;

/// One line per item, prefixed with `ok` or `WARN`.
pub fn report(config: &Config, secrets: &Secrets) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = |ok: bool, text: String| {
        lines.push(format!("{} {text}", if ok { "ok  " } else { "WARN" }));
    };

    for file in ["stat", "meminfo", "loadavg", "uptime"] {
        let path = config.paths.proc_root.join(file);
        line(
            fs::read_to_string(&path).is_ok(),
            format!("read {}", path.display()),
        );
    }
    for mount in &config.host.mounts {
        let usage = mount_usage(mount);
        line(
            usage.total_bytes.is_some(),
            format!("statvfs {}", mount.display()),
        );
    }
    for service in &config.services {
        let dir = config.paths.cgroup_root.join(service.cgroup_rel());
        let running = fs::read_to_string(dir.join("pids.current"))
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok())
            .is_some_and(|n| n > 0);
        let text = match (dir.is_dir(), running) {
            (true, true) => format!(
                "service {}: cgroup {} (running)",
                service.name,
                dir.display()
            ),
            (true, false) => format!(
                "service {}: cgroup {} (no processes)",
                service.name,
                dir.display()
            ),
            (false, _) => format!(
                "service {}: cgroup {} not found (unit not running, or wrong unit name)",
                service.name,
                dir.display()
            ),
        };
        line(dir.is_dir(), text);
        for path in &service.paths {
            let readable =
                fs::read_dir(path).is_ok() || (path.is_file() && fs::metadata(path).is_ok());
            line(
                readable,
                format!(
                    "service {}: data path {}{}",
                    service.name,
                    path.display(),
                    if readable {
                        ""
                    } else {
                        " is not readable by this user"
                    }
                ),
            );
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
    use std::path::Path;

    #[test]
    fn reports_fixture_tree() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let config = Config::parse(&format!(
            r#"
            [paths]
            proc_root = "{root}/proc"
            cgroup_root = "{root}/cgroup"
            [host]
            mounts = ["/"]
            [[service]]
            name = "cloudflared"
            unit = "cloudflared.service"
            paths = ["{root}"]
            [[service]]
            name = "site"
            unit = "site.service"
            paths = ["/definitely/not/here"]
            [discord]
            enabled = true
            "#,
            root = root.display()
        ))
        .unwrap();
        let lines = report(&config, &Secrets::default());
        let warns: Vec<_> = lines.iter().filter(|l| l.starts_with("WARN")).collect();
        assert!(
            lines.iter().any(|l| l.starts_with("ok")
                && l.contains("cloudflared")
                && l.contains("(running)"))
        );
        assert_eq!(warns.len(), 3, "{lines:#?}");
        assert!(warns[0].contains("site") && warns[0].contains("not found"));
        assert!(warns[1].contains("not readable"));
        assert!(warns[2].contains(DISCORD_WEBHOOK_ENV));
        assert!(lines.iter().all(|l| !l.contains("http")));
    }
}
