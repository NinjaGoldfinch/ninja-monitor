# ninja-monitor

[![CI](https://github.com/NinjaGoldfinch/ninja-monitor/actions/workflows/ci.yml/badge.svg)](https://github.com/NinjaGoldfinch/ninja-monitor/actions/workflows/ci.yml)

A small Rust daemon that watches one Linux VPS. It samples CPU, memory and disk
usage for the host and for each configured systemd service, keeps recent
history in memory, serves it as JSON on localhost, sends alerts to a Discord
webhook and can ping an external heartbeat checker. Optionally it pushes
everything to [ninja-ingest](ingest/README.md), a small central server in
this repository that keeps weeks of history for several machines.

It aims to be the lightest thing on the box:

- One small binary (about 2 MiB), one process, a current-thread async runtime.
- About 2 MiB RSS at start and under 8 MiB with a full day of history
  (host, one mount and three services at 15 second samples).
- Effectively no CPU between samples. Nothing is written to disk.

It needs systemd and cgroup v2, which every current Ubuntu release uses.

## Contents

- [What the numbers mean](#what-the-numbers-mean)
- [Setup](#setup)
- [Reaching the API](#reaching-the-api)
- [Configuration](#configuration)
- [Alerts](#alerts)
- [Pushing to a central server](#pushing-to-a-central-server)
- [API reference](#api-reference)
- [Permissions and hardening](#permissions-and-hardening)
- [Development](#development)

## What the numbers mean

### Host

| Field | Meaning |
|---|---|
| `cpu_percent` | Busy time across all cores since the previous sample, 0 to 100. `null` on the first sample. |
| `cores` | Number of CPU cores. |
| `load1`, `load5`, `load15` | Load averages. Compare them with `cores`. |
| `mem_total_bytes` | Installed memory. |
| `mem_available_bytes` | Memory the kernel can hand out right now without swapping, including cache it can drop. **This is the number to watch.** |
| `mem_used_bytes` | `total - available`. |
| `mem_cache_bytes` | Page cache, buffers and reclaimable slab. Mostly free to reuse. |
| `swap_total_bytes`, `swap_used_bytes` | Swap, if any. |
| `uptime_secs` | Time since boot. |
| `disks[]` | Per configured mount: `total_bytes`, `used_bytes`, `avail_bytes` and `used_percent` (computed the same way as `df`). |

### Services

Each service maps to its cgroup, by default `/sys/fs/cgroup/system.slice/<unit>`.

| Field | Meaning |
|---|---|
| `state` | `running` if the cgroup exists and has processes, otherwise `down`. |
| `cpu_percent` | Percent of **one** core since the previous sample, so 250 means two and a half cores. `null` on the first sample and right after a restart. |
| `mem_anon_bytes` | The service's own memory (heap, stacks). **The headline figure.** |
| `mem_file_bytes` | Page cache charged to the service, such as memory-mapped files. |
| `mem_current_bytes` | Everything charged to the cgroup: anon, file cache and kernel memory. |
| `mem_peak_bytes` | Highest `mem_current_bytes` since the service started (kernel 5.19+). |
| `oom_kills` | Times the kernel OOM-killed a process in this service. |
| `pids` | Number of processes and threads. |
| `restarts_seen` | Restarts noticed by ninja-monitor (the CPU counter went backwards). |
| `disk_bytes`, `disk_measured_at` | Space used under the service's configured `paths`, and when it was measured. Updated every `disk_interval`. |

### Anon versus cache

`mem_current_bytes` includes page cache, which the kernel reclaims whenever
something else needs memory. A service with a large memory-mapped database can
look huge by that number while its real footprint is small. For example, a
proxy might show 590 MiB current but only 200 MiB anon; the other 390 MiB is
cached file data that costs nothing to drop.

So use `mem_anon_bytes` for a service's memory, and `mem_available_bytes`
(not "used including cache") for the host. The example alert rules do exactly
that.

Disk usage counts allocated blocks (like `du`), does not follow symlinks below
a configured path, stays on one filesystem and counts hard-linked files once.
Any missing or unreadable value is reported as `null`, never as 0.

## Setup

Everything happens on the server, over SSH. Each
[release](https://github.com/NinjaGoldfinch/ninja-monitor/releases) has a
static binary for x86_64 and ARM, bundled with the install script, so the
server needs no compiler:

```sh
arch=$(uname -m)   # x86_64 or aarch64
base=https://github.com/NinjaGoldfinch/ninja-monitor/releases/latest/download
curl -fsSLO "$base/ninja-monitor-$arch-linux.tar.gz"
curl -fsSLO "$base/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf "ninja-monitor-$arch-linux.tar.gz"
cd ninja-monitor
sudo ./install.sh --webhook --start   # asks for the Discord webhook, input hidden
```

That's it. On first install, `install.sh` writes
`/etc/ninja-monitor/config.toml` for you from the services that are running
right now. It skips the operating system's own units (journald, cron, ssh and
so on), and picks up each service's systemd `StateDirectory` as a data path.
Every detected service gets three alert rules: anonymous memory above half the
machine's RAM for 5 minutes, down for 1 minute, and any OOM kill. The host gets
"memory available below 10%" and "disk above 85%".

Before starting, it checks the whole setup as the service's own user, with
your secrets loaded, and prints one `ok` or `WARN` line per item. It
refuses to start if the config is invalid.

Releases are built by GitHub Actions from the tagged commit and carry a signed
build provenance attestation. If you have the GitHub CLI, you can confirm a
download came from this repository's workflow:

```sh
gh attestation verify "ninja-monitor-$arch-linux.tar.gz" --repo NinjaGoldfinch/ninja-monitor
```

### Everyday commands

All of these run on the server, from the extracted `ninja-monitor` directory.
(In a source checkout, the script is `./deploy/install.sh`.)

| Task | Command |
|---|---|
| Upgrade | Run the download commands above again, then `sudo ./install.sh --start` |
| Change the webhook | `sudo ./install.sh --webhook --start` |
| Turn on the heartbeat | `sudo ./install.sh --heartbeat --start` |
| Push to a ninja-ingest server | `sudo ./install.sh --push --start` (asks for the URL and the token) |
| Edit the config | `sudoedit /etc/ninja-monitor/config.toml && sudo ./install.sh --start` |
| Re-detect services | `sudo ./install.sh --reconfigure --start` (the old config is kept as `config.toml.bak`) |
| Check the setup | `sudo ./install.sh` (installs the same binary again, starts nothing) |
| Status and logs | `systemctl status ninja-monitor` and `journalctl -u ninja-monitor -f` |
| Remove it | `sudo ./install.sh --uninstall` (keeps `/etc/ninja-monitor`) |

`install.sh` is safe to run any number of times. It never overwrites the
config or the env file unless you ask (`--reconfigure`, `--webhook`,
`--heartbeat`, `--push`), and it only starts or restarts the service with
`--start`.

What it sets up:

- a `ninja-monitor` system user (no login, no home),
- the binary at `/usr/local/bin/ninja-monitor`,
- `/etc/ninja-monitor/config.toml` (mode 0640, readable by the service user),
- `/etc/ninja-monitor/env` for secrets (mode **0600**, root only; systemd reads
  it before dropping privileges),
- the systemd unit `ninja-monitor.service`.

To watch a service that was not running at install time, or one the detection
skipped, add a `[[service]]` block to the config (see
[Configuration](#configuration)). `systemctl list-units --type=service
--state=running` lists unit names, and `systemd-cgtop -1` shows which ones have
a cgroup under `/system.slice/`.

### Building from source on the server

If you prefer to build it yourself, or want unreleased changes:

```sh
sudo apt-get update && sudo apt-get install -y build-essential git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
git clone https://github.com/NinjaGoldfinch/ninja-monitor.git
cd ninja-monitor
cargo build --release
sudo ./deploy/install.sh --webhook --start
```

To upgrade: `git pull && cargo build --release && sudo ./deploy/install.sh --start`.
Building needs up to about 900 MiB of free memory for a minute or two.
The repository also holds the ninja-ingest server, and `cargo build --release`
builds both; on a small box add `-p ninja-monitor` to build only the agent and
skip compiling SQLite.

#### Build memory

Compiling is the only expensive part, and only while `cargo build` runs.
Most of the peak is one step: the final optimisation of ninja-monitor itself,
which `lto = true` and `codegen-units = 1` make heavy in exchange for a smaller
binary. Running fewer jobs at once therefore helps less than you might expect.

Measured on Ubuntu 24.04 (Rust 1.99), as the peak combined memory of all build
processes:

| Command | Peak memory | Binary |
|---|---|---|
| `cargo build --release` on 2 cores (the default) | 830 to 900 MiB | 1.9 MiB |
| `cargo build --release -j 1` | 820 MiB | 1.9 MiB |
| `CARGO_PROFILE_RELEASE_LTO=thin cargo build --release -j 1` | 580 MiB | 2.4 MiB |
| `CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release -j 1` | 500 MiB | 2.8 MiB |

The last two only change this one build; nothing in the repository needs
editing. On a machine without swap that is short of memory, use the last
command, or build somewhere else (below). On a fast machine `-j 1` roughly
doubles the build time, from about 15 to about 30 seconds; a small VPS will be
slower.

#### Removing the build files

`install.sh` copies the binary to `/usr/local/bin`, so nothing the service uses
lives in the checkout or the Rust toolchain. Once installed you can reclaim
the space:

| Command | Frees | Note |
|---|---|---|
| `cargo clean` | about 200 MB | Build output in `target/`. |
| `rustup self uninstall` | about 730 MB | The Rust toolchain and download cache (`~/.rustup`, `~/.cargo`). |
| `cd .. && rm -rf ninja-monitor` | a few MB | The checkout. |

Keep the checkout if you can: its `deploy/install.sh` is how you change the
webhook, regenerate the config or uninstall. If you delete it, clone it again
or download a release when you need those. Upgrading from source means
building again, so reinstall Rust first.

### Building on another machine

To cross-compile a static binary yourself and pass its path to `install.sh`
(this is what the releases do, natively). From macOS, with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild):

```sh
rustup target add x86_64-unknown-linux-musl
brew install zig
cargo install --locked cargo-zigbuild
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

Copy `target/x86_64-unknown-linux-musl/release/ninja-monitor` and the `deploy/`
directory to the server, then run
`sudo ./deploy/install.sh --webhook --start path/to/ninja-monitor`. Use the
`aarch64-unknown-linux-musl` target for an ARM server (`uname -m` prints
`aarch64`). zig may print a harmless `ignoring deprecated linker optimization
setting` warning.

## Reaching the API

The API only listens on `127.0.0.1:9090`. Forward it over SSH:

```sh
ssh -N -L 9090:localhost:9090 <user>@<vps>
```

Then, on your own machine:

```sh
curl -s localhost:9090/stats | python3 -m json.tool
```

## Configuration

One TOML file, `/etc/ninja-monitor/config.toml` by default (change it with
`--config <path>`). `install.sh` generates it on first install;
`ninja-monitor --init` prints a fresh one for the services running now. It is checked at startup and the daemon refuses to start
with a clear message if anything is wrong, including unknown keys. Durations
are whole numbers with a unit: `ms`, `s`, `m`, `h` or `d`.
[`deploy/config.example.toml`](deploy/config.example.toml) is a full,
commented example.

| Option | Default | Meaning |
|---|---|---|
| `[server] bind` | `127.0.0.1:9090` | Address for the API. |
| `[server] allow_non_loopback` | `false` | Must be `true` to bind anything other than loopback. |
| `[sampling] interval` | `15s` | Time between samples. |
| `[sampling] disk_interval` | `10m` | Time between per-service disk usage walks. A walk still running when the next is due skips that round. |
| `[sampling] history` | `24h` | How much history to keep in memory. Lost on restart. |
| `[paths] cgroup_root` | `/sys/fs/cgroup` | Where cgroup v2 is mounted. |
| `[paths] proc_root` | `/proc` | Where procfs is mounted. |
| `[host] name` | `vps` | Name used in alert messages. |
| `[host] mounts` | `["/"]` | Filesystems to report space for. |
| `[[service]] name` | required | Short name used in the API and alerts. |
| `[[service]] unit` | required | systemd unit name, such as `nginx.service`. |
| `[[service]] paths` | `[]` | Directories (or files) whose disk usage counts towards the service. |
| `[[service]] cgroup` | `system.slice/<unit>` | Cgroup path relative to `cgroup_root`, for units outside `system.slice`. |
| `[discord] enabled` | `false` | Send alerts to the webhook in `NINJA_MONITOR_DISCORD_WEBHOOK`. If the variable is missing, a warning is logged and the daemon runs without notifications. |
| `[discord] startup_message` | `true` | Post a `STARTED` message at every start, so a restart loop is visible. |
| `[heartbeat] enabled` | `false` | Send a GET to a URL on a timer. |
| `[heartbeat] url_env` | `NINJA_MONITOR_HEARTBEAT_URL` | Environment variable holding the heartbeat URL. |
| `[heartbeat] interval` | `60s` | Time between heartbeats. Failures are logged and otherwise ignored. |
| `[push] enabled` | `false` | Push samples and alert events to a ninja-ingest server, see [below](#pushing-to-a-central-server). |
| `[push] url` | required when enabled | The server's ingest URL. `https://`, or `http://` to a loopback address. |
| `[push] token_env` | `NINJA_MONITOR_PUSH_TOKEN` | Environment variable holding the bearer token. Missing: a warning is logged and the daemon runs without pushing. |
| `[push] flush_interval` | the sampling interval | Minimum time between pushes. |
| `[push] buffer` | `1h` | How much to keep while the server is unreachable. Oldest samples go first when full. At most 10 000 samples. |
| `[push] alert_after` | `10m` | Discord alert when no push has succeeded for this long. `0s` disables it. |
| `[[alert]]` | none | Alert rules, see below. |

Environment variables (set them in `/etc/ninja-monitor/env`, or let
`install.sh --webhook` / `--heartbeat` do it):

| Variable | Meaning |
|---|---|
| `NINJA_MONITOR_DISCORD_WEBHOOK` | Discord webhook URL. Secret. |
| `NINJA_MONITOR_HEARTBEAT_URL` | Heartbeat URL (or whatever `url_env` names). Secret. |
| `NINJA_MONITOR_PUSH_TOKEN` | Bearer token for the ninja-ingest server (or whatever `[push] token_env` names). Secret. |
| `NINJA_MONITOR_LOG` | `error`, `warn`, `info` (default), `debug` or `trace`. |

Secrets are only ever read from the environment. They are never written to
logs, API responses or error messages.

## Alerts

Each rule is evaluated against every sample:

```text
ok --(condition true)--> pending --(true for the whole `for`)--> firing
pending --(condition false)--> ok
firing --(condition false)--> ok, sending a RESOLVED message
```

A rule sends one message when it starts firing and one when it resolves. Set
`repeat = "6h"` to be reminded while it keeps firing. A rule without `for`
fires on the first matching sample. If a value is missing (`null`), the rule's
state does not change.

```toml
[[alert]]
name = "riot-proxy memory high"   # unique; shown in messages
scope = "service:riot-proxy"      # "host" or "service:<name>"
metric = "mem_anon_bytes"
op = ">"                          # >, >=, <, <=
threshold = 2000000000
for = "5m"                        # optional
repeat = "6h"                     # optional
```

| Scope | Metrics |
|---|---|
| `host` | `cpu_percent`, `load1`, `mem_available_percent`, `mem_available_bytes`, `swap_used_percent`, `disk_used_percent` (needs `mount = "/..."`, which must be listed in `host.mounts`) |
| `service:<name>` | `cpu_percent`, `mem_anon_bytes`, `mem_current_bytes`, `pids`, `disk_bytes`, `down`, `oom_kill` |

Two metrics take no `op` or `threshold`:

- `down` is true while the service is not running. Missing data counts as down.
- `oom_kill` is an event: it fires once each time the kernel OOM-kills a process
  in the service and never sends a RESOLVED message.

`swap_used_percent` is `null` on a machine without swap, so those rules never
fire there.

Messages look like this:

```text
**FIRING** [vps] riot-proxy memory high: mem_anon_bytes = 2.0 GiB (threshold > 1.9 GiB), for 5m 12s
**RESOLVED** [vps] riot-proxy memory high: mem_anon_bytes = 1.2 GiB (threshold > 1.9 GiB), lasted 7m 40s
```

Delivery happens on its own queue (up to 100 messages, oldest dropped first),
so a slow or failing Discord never delays sampling. Each message is retried up
to 3 times with exponential backoff, and rate limits wait for Discord's
`retry_after`. Messages never ping anyone.

## Pushing to a central server

The agent keeps a day of history in memory, on the very machine whose failure
you most want to notice. [ninja-ingest](ingest/README.md) is a second small
daemon, in this repository, that runs somewhere else and receives every
sample and alert event from one or more agents, keeps them for 30 days in
SQLite and serves them with the same API shapes plus a host dimension.

On the server, `sudo ./install-ingest.sh --add-agent vps` prints a token.
Then here:

```sh
sudo ./install.sh --push --start   # asks for the server's URL and the token
```

That writes the token to `/etc/ninja-monitor/env` and a `[push]` section to
the config. From then on every sample is sent as it is taken. While the
server is unreachable the agent buffers an hour of samples (oldest dropped
first beyond that), retries with back-off of 15, 30 then 60 seconds, and
after `alert_after` (10 minutes) posts to Discord:

```text
**FIRING** [vps] ingest unreachable: no successful push for 10m 5s (42 samples buffered)
**RESOLVED** [vps] ingest unreachable: reachable again after 12m, 48 samples caught up
```

The server, for its part, posts `SILENT` when this agent stops reporting.
Between the two, both directions are watched, so **once pushing is on, the
agent's `[heartbeat]` is redundant** and can be turned off. It keeps working
for anyone running the agent alone. `sudo ./install.sh` (the check step)
confirms the token is set and the server's `/healthz` answers.

Nothing else changes: the local API, history and alert rules stay as they
are. The pusher is one more subscriber on the sample bus and can never slow
sampling or alerting.

## API reference

All responses are JSON and all timestamps are Unix milliseconds (UTC).
Errors return `{"error": "..."}`.

### `GET /healthz`

```json
{"status":"ok","version":"0.1.0","uptime_secs":3600}
```

`uptime_secs` is how long ninja-monitor has been running.

### `GET /stats`

The latest sample: `timestamp`, a `host` object and a `services` array with
the fields described in [What the numbers mean](#what-the-numbers-mean).
Returns 503 until the first sample is taken (a moment after start).

### `GET /history`

| Parameter | Default | Meaning |
|---|---|---|
| `scope` | required | `host` or `service:<name>`. Unknown scopes return 404. |
| `minutes` | `60` | Window ending now. Capped at the configured `history`. |
| `max_points` | `720` | If the window holds more samples, they are averaged in fixed-size buckets. |

The response is columnar: one array per field, all aligned with a shared `t`
array. `bucket` says how many samples were averaged into each point.

```json
{
  "scope": "service:riot-proxy", "from": 1760000000000, "to": 1760003600000,
  "minutes": 60, "bucket": 1,
  "t": [1760003570000, 1760003585000],
  "up": [1.0, 1.0],
  "cpu_percent": [12.5, 9.75],
  "pids": [14, 14],
  "mem_current_bytes": [618397696, 620000000],
  "mem_anon_bytes": [208506880, 209000000],
  "mem_file_bytes": [403083264, 404000000],
  "oom_kills": [0, 0],
  "disk_bytes": [73728, 73728]
}
```

Host history has `cpu_percent`, `load1`, `load5`, `load15`,
`mem_available_bytes`, `mem_used_bytes`, `mem_cache_bytes`, `swap_used_bytes`
and a `disks` array with `used_bytes` and `used_percent` for each mount.
For services, `up` is the fraction of samples in the bucket where the service
was running, and `oom_kills` is the highest count in the bucket. Missing values
are skipped when averaging; a bucket with no values is `null`.

### `GET /alerts`

```json
{
  "rules": [
    {"name": "riot-proxy down", "scope": "service:riot-proxy", "metric": "down",
     "op": null, "threshold": null, "for_secs": 60,
     "state": "ok", "since": null, "value": 0.0}
  ],
  "events": [
    {"timestamp": 1760003585000, "rule": "riot-proxy down", "scope": "service:riot-proxy",
     "metric": "down", "kind": "resolved", "value": 0.0, "op": null,
     "threshold": null, "held_secs": 95}
  ]
}
```

`state` is `ok`, `pending` or `firing`, and `since` is when the current pending
or firing state began. `events` holds the last 100 events, newest first;
`kind` is `firing`, `repeat` or `resolved`.

## Permissions and hardening

The service runs as the unprivileged `ninja-monitor` user with a hardened unit
(`systemd-analyze security` rates it 1.3, "OK"). It needs no write access
anywhere. The unit also caps memory at `MemoryMax=64M`.

Everything in `/proc` and `/sys/fs/cgroup` that ninja-monitor reads is
world-readable, so host and service metrics need no extra rights. **Disk usage
for a service's `paths` does**: the `ninja-monitor` user can only measure what
it can read. Unreadable parts are skipped, the total becomes a lower bound and
a warning is logged once per path. Running `sudo ./install.sh` shows
which paths are unreadable.

To let it read a data directory, pick one:

- **Add the owning group** (works when the directory is group-readable, such as
  mode 0750). Use a drop-in so upgrades keep it:
  ```sh
  sudo systemctl edit ninja-monitor
  ```
  ```ini
  [Service]
  SupplementaryGroups=riot-proxy
  ```
  This also gives ninja-monitor read access to everything else that group can
  read, so only add groups you are comfortable with.
- **Grant read access with an ACL** (works for 0700 directories, and limits the
  grant to that tree):
  ```sh
  sudo setfacl -R -m u:ninja-monitor:rX /var/lib/riot-proxy
  sudo setfacl -R -d -m u:ninja-monitor:rX /var/lib/riot-proxy
  ```

Paths under `/home` are hidden by `ProtectHome=yes`. If a service keeps its
data there, add `ProtectHome=read-only` in the same drop-in. Running as root
would also work but is not recommended.

## Development

Everything builds and tests on macOS. The kernel files are read from
configurable roots, and the tests use fake `proc` and cgroup trees in
[`tests/fixtures`](tests/fixtures), copied from a real Ubuntu server.

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Run it locally against moving fixture data:

```sh
dev/simulate.sh                          # terminal 1: copies the fixtures to dev/live and keeps changing them
cargo run -- --config dev/config.toml    # terminal 2
curl -s localhost:9090/stats             # terminal 3
```

Set `NINJA_MONITOR_DISCORD_WEBHOOK` before `cargo run` to try alerts against a
real channel. The dev config has rules that fire and resolve on their own.

### Continuous integration and releases

Every push and pull request runs formatting, clippy and the tests on Linux and
macOS, plus shellcheck on the scripts
([`ci.yml`](.github/workflows/ci.yml)). To publish a release:

1. Bump `version` in `Cargo.toml`, run `cargo build` to update `Cargo.lock`,
   and commit.
2. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`.

[`release.yml`](.github/workflows/release.yml) then runs the checks again,
builds static binaries for both the agent and ninja-ingest natively on
x86_64 and ARM runners, smoke-tests them, attests their provenance and
publishes the release with `SHA256SUMS`. It refuses a tag that does not
match the version in both `Cargo.toml` files; bump `ingest/Cargo.toml`
together with the root one.

### Design

One process, one broadcast channel. The sampler builds an immutable sample
every tick and publishes it; the store, the alert engine and the pusher are
independent subscribers, so a slow consumer can never stall sampling. The
disk walker runs on its own slow timer on a blocking thread and hands its
results to the sampler.

The repository is a Cargo workspace: this crate at the root and the
ninja-ingest server in [`ingest/`](ingest/), which depends on this one for
the sample types (the wire format is the agent's own JSON) and the history
code, so the two APIs stay identical by construction.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
