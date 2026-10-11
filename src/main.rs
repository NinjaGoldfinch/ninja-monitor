use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;
use tracing::level_filters::LevelFilter;

use ninja_monitor::config::{Config, DEFAULT_CONFIG_PATH, Secrets};

const USAGE: &str = "\
Usage: ninja-monitor [--config <path>]

Options:
  --config <path>  Config file (default /etc/ninja-monitor/config.toml)
  --check          Validate the config, check the paths it names, then exit
  --init           Print a starting config for the services running here
  -h, --help       Show this help
  -V, --version    Show the version

Environment:
  NINJA_MONITOR_LOG              Log level: error, warn, info (default), debug, trace
  NINJA_MONITOR_DISCORD_WEBHOOK  Discord webhook URL";

enum Command {
    Run(PathBuf),
    Check(PathBuf),
    Init,
    Help,
    Version,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut config = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--init" => return Ok(Command::Init),
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--config" => {
                config = args.next().ok_or("--config needs a path")?.into();
            }
            _ => match arg.strip_prefix("--config=") {
                Some(path) => config = path.into(),
                None => return Err(format!("unknown argument `{arg}`")),
            },
        }
    }
    Ok(if check {
        Command::Check(config)
    } else {
        Command::Run(config)
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let config_path = match parse_args(std::env::args().skip(1)) {
        Ok(Command::Run(path)) => path,
        Ok(Command::Check(path)) => return check(&path),
        Ok(Command::Init) => {
            use ninja_monitor::init;
            let (proc_root, cgroup_root) = (Path::new("/proc"), Path::new("/sys/fs/cgroup"));
            let services = init::detect(cgroup_root, init::systemd_state_paths);
            let threshold = init::memory_threshold(proc_root);
            print!(
                "{}",
                init::render(&init::host_name(proc_root), &services, threshold)
            );
            return ExitCode::SUCCESS;
        }
        Ok(Command::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("ninja-monitor {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("ninja-monitor: {msg}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    init_logging();
    match run(config_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ninja-monitor: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn check(path: &std::path::Path) -> ExitCode {
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("ninja-monitor: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    println!("config {} is valid", path.display());
    let secrets = Secrets::from_env(&config);
    for line in ninja_monitor::check::report(&config, &secrets) {
        println!("{line}");
    }
    ExitCode::SUCCESS
}

async fn run(config_path: PathBuf) -> anyhow::Result<()> {
    let config = Config::load(&config_path)?;
    let secrets = Secrets::from_env(&config);
    let listener = TcpListener::bind(config.server.bind)
        .await
        .with_context(|| format!("cannot listen on {}", config.server.bind))?;

    let mut sigterm = signal(SignalKind::terminate()).context("cannot install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot install SIGINT handler")?;
    let shutdown = async move {
        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM, shutting down"),
            _ = sigint.recv() => info!("received SIGINT, shutting down"),
        }
    };

    info!(
        bind = %config.server.bind,
        services = config.services.len(),
        interval = ?config.sampling.interval,
        "ninja-monitor {} started",
        env!("CARGO_PKG_VERSION")
    );
    ninja_monitor::run(config, secrets, listener, shutdown).await?;
    info!("stopped");
    Ok(())
}

/// Plain stderr output without timestamps; journald adds its own.
fn init_logging() {
    let level = std::env::var("NINJA_MONITOR_LOG")
        .ok()
        .and_then(|v| v.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn args() {
        assert!(matches!(parse(&[]), Ok(Command::Run(p)) if p == Path::new(DEFAULT_CONFIG_PATH)));
        assert!(
            matches!(parse(&["--config", "a.toml"]), Ok(Command::Run(p)) if p == Path::new("a.toml"))
        );
        assert!(
            matches!(parse(&["--config=b.toml"]), Ok(Command::Run(p)) if p == Path::new("b.toml"))
        );
        assert!(matches!(parse(&["--help"]), Ok(Command::Help)));
        assert!(matches!(parse(&["--init"]), Ok(Command::Init)));
        assert!(
            matches!(parse(&["--check", "--config", "c.toml"]), Ok(Command::Check(p)) if p == Path::new("c.toml"))
        );
        assert!(matches!(parse(&["-V"]), Ok(Command::Version)));
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}
