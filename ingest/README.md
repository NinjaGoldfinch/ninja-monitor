# ninja-ingest

The central store for [ninja-monitor](../README.md) agents. Each agent pushes
its samples and alert events here over HTTPS; ninja-ingest keeps them in one
SQLite file for 30 days, serves them as JSON with the same shapes as the
agent's own API plus a host dimension, and tells Discord when an agent goes
quiet.

It runs on a machine that is unlikely to die together with the boxes it
watches: a home server, a Proxmox node, a second VPS. cloudflared (or any
reverse proxy) sits in front for TLS and a public name; ninja-ingest itself
listens on loopback. The agents only ever connect outward.

- One static binary (about 3 MiB with SQLite built in), one process, one
  database file. Under 30 MiB RSS with a few agents reporting.
- Nothing is lost quietly: agents buffer while this server is unreachable
  and alert through Discord if it stays that way; this server alerts when an
  agent stops reporting.

## Contents

- [How the pieces fit](#how-the-pieces-fit)
- [Setup](#setup)
- [Running it in a Proxmox container](#running-it-in-a-proxmox-container)
- [Putting it behind cloudflared](#putting-it-behind-cloudflared)
- [Configuration](#configuration)
- [Tokens](#tokens)
- [Who notices what](#who-notices-what)
- [API reference](#api-reference)
- [Storage and disk use](#storage-and-disk-use)
- [Backup and uninstall](#backup-and-uninstall)
- [Development](#development)

## How the pieces fit

```
VPS:  ninja-monitor --(HTTPS POST, bearer token)--> cloudflared --> ninja-ingest --> SQLite
                                                                          |
                                      frontend / curl  <-- GET /hosts ... +--> Discord (SILENT / REPORTING)
                                                                          +--> heartbeat checker
```

Each agent keeps its own day of history and its own alert rules; nothing
about that changes. With `[push]` enabled it also sends every sample, and
every alert transition, to this server as it happens. If the server is
unreachable the agent buffers (an hour by default) and catches up later, so
the history here has no holes after an outage, only an alert.

## Setup

Everything happens on the server, over SSH. Each
[release](https://github.com/NinjaGoldfinch/ninja-monitor/releases) has a
static binary for x86_64 and ARM bundled with the install script:

```sh
arch=$(uname -m)   # x86_64 or aarch64
base=https://github.com/NinjaGoldfinch/ninja-monitor/releases/latest/download
curl -fsSLO "$base/ninja-ingest-$arch-linux.tar.gz"
curl -fsSLO "$base/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf "ninja-ingest-$arch-linux.tar.gz"
cd ninja-ingest
sudo ./install-ingest.sh --webhook          # asks for the Discord webhook, input hidden
sudo ./install-ingest.sh --add-agent vps    # prints the agent's token once
sudo ./install-ingest.sh --start
```

`--add-agent` writes an `[[agent]]` block to the config and a fresh random
token to the env file, then prints the token. Carry it to the agent:

```sh
# on the VPS
sudo ./install.sh --push --start   # asks for this server's ingest URL and the token
```

Within a minute `curl -s localhost:9091/hosts` here shows the host as
`reporting`. Repeat `--add-agent` for every box; run it again for a host
that already exists to rotate its token.

Before starting, the script checks the whole setup as the service's own
user, with your secrets loaded, and prints one `ok` or `WARN` line per item.
It refuses to start if the config is invalid.

Releases carry a signed build provenance attestation:

```sh
gh attestation verify "ninja-ingest-$arch-linux.tar.gz" --repo NinjaGoldfinch/ninja-monitor
```

### Everyday commands

All run on the server, from the extracted `ninja-ingest` directory. (In a
source checkout the script is `./deploy/install-ingest.sh`.)

| Task | Command |
|---|---|
| Upgrade | Run the download commands above again, then `sudo ./install-ingest.sh --start` |
| Add an agent, or rotate its token | `sudo ./install-ingest.sh --add-agent <name> --start` |
| Change the webhook | `sudo ./install-ingest.sh --webhook --start` |
| Turn on the heartbeat | `sudo ./install-ingest.sh --heartbeat --start` |
| Edit the config | `sudoedit /etc/ninja-ingest/config.toml && sudo ./install-ingest.sh --start` |
| Check the setup | `sudo ./install-ingest.sh` (installs the same binary again, starts nothing) |
| Status and logs | `systemctl status ninja-ingest` and `journalctl -u ninja-ingest -f` |
| Remove it | `sudo ./install-ingest.sh --uninstall` (keeps the config and the database) |

What it sets up:

- a `ninja-ingest` system user (no login, no home),
- the binary at `/usr/local/bin/ninja-ingest`,
- `/etc/ninja-ingest/config.toml` (mode 0640, readable by the service user),
- `/etc/ninja-ingest/env` for the tokens and webhook (mode **0600**, root
  only; systemd reads it before dropping privileges),
- `/var/lib/ninja-ingest/` for the database (owned by the service user, the
  only place the daemon may write),
- the systemd unit `ninja-ingest.service`.

On a Proxmox node, [`proxmox-setup.sh`](#running-it-in-a-proxmox-container)
does all of this inside a fresh container for you.

### Building from source

Same as the agent: `cargo build --release` in a checkout builds both
binaries (the server compiles its own copy of SQLite, which adds about a
minute), then `sudo ./deploy/install-ingest.sh --start`. To build only the
server, `cargo build --release -p ninja-ingest`. The agent README covers
cross-compiling a static binary from macOS; use the same command with
`-p ninja-ingest`.

## Running it in a Proxmox container

The natural home for ninja-ingest on a Proxmox node is a small unprivileged
LXC container: Debian 12, one core, 512 MiB, an 8 GiB disk, no swap.
[`proxmox-setup.sh`](../deploy/proxmox-setup.sh) builds that container and
everything inside it in one go. Only the script is needed on the Proxmox
host; it downloads the release inside the container. Run it as root:

```sh
curl -fsSLO https://raw.githubusercontent.com/NinjaGoldfinch/ninja-monitor/main/deploy/proxmox-setup.sh
sh proxmox-setup.sh --agent vps
```

(The same script ships in the `ninja-ingest` release tarball.)

It asks for the Discord webhook and a Cloudflare Tunnel token (both typed
hidden, both optional), then without further input:

1. downloads the newest Debian 12 template if the node does not have it,
2. creates the container with the next free id, `nesting=1` (so systemd
   can set up the hardened unit's namespaces inside an unprivileged
   container), `onboot`, and DHCP on `vmbr0`,
3. installs ninja-ingest from the release inside it, adds each `--agent`,
   stores the webhook and starts the service,
4. installs cloudflared from Cloudflare's apt repository and enrols it in
   your tunnel with the token,
5. prints the agents' tokens once, and the next steps.

Secrets reach the container through a root-only file that is deleted
afterwards, never through a command line. Options cover the usual knobs:
`--id`, `--hostname`, `--storage`, `--bridge`, a static `--ip
192.168.1.50/24,gw=192.168.1.1`, `--memory`, `--disk`, and `--release
v0.2.0` to pin a version. `--help` lists them all.

Afterwards the container is an ordinary one: `pct enter <id>` for a shell,
and `pct exec <id> -- /root/ninja-ingest/install-ingest.sh --add-agent
<name> --start` to add a host later. Upgrades are the normal download and
`install-ingest.sh --start` inside it.

### One container or two?

By default cloudflared runs **inside the same container**, talking to
ninja-ingest over loopback. Nothing in the container listens on the
network, and the two services are useless without each other anyway, so
sharing a container costs nothing. This is the right choice for a tunnel
dedicated to ninja-ingest.

Run cloudflared **in a separate container** only if you already have one
fronting other services on the node. One tunnel can carry many public
hostnames, so adding `ingest.example.net` to it is less to manage than a
second tunnel, and cloudflared's frequent updates stay away from the
container holding your database. The cost is that ninja-ingest must then
listen on its container's bridge address rather than loopback, and the hop
between the containers is plain HTTP on your LAN, where the read routes have
no authentication of their own. For that layout use:

```sh
sh proxmox-setup.sh --agent vps --expose
```

which skips cloudflared, binds ninja-ingest on `0.0.0.0:9091` with
`allow_non_loopback = true`, and prints the container's address to point
the existing tunnel's public hostname at.

### Doing it by hand

The script is a convenience. The equivalent by hand is `pct create` with
the flags above, then inside the container the [Setup](#setup) steps, then
the cloudflared install from the [Cloudflare
docs](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/)
and `cloudflared service install <token>`.

## Putting it behind cloudflared

ninja-ingest only listens on `127.0.0.1:9091`. Give it a public name with a
Cloudflare Tunnel. With a remote-managed tunnel (the kind
`proxmox-setup.sh` enrols with a token) the routing lives in the Zero Trust
dashboard: Networks > Tunnels > your tunnel > Public Hostname, add
`ingest.example.net` with service `http://localhost:9091`. With a locally
configured tunnel, the same thing in `/etc/cloudflared/config.yml`:

```yaml
ingress:
  - hostname: ingest.example.net
    service: http://localhost:9091
  - service: http_status:404
```

Then protect the read routes with Cloudflare Access while leaving the ingest
route open to the agents, which bring their own bearer tokens. In Zero Trust,
create two Access applications for the same hostname:

| Application path | Policy |
|---|---|
| `ingest.example.net/v1/ingest` | Bypass, Everyone |
| `ingest.example.net` | Allow, your identity (email one-time PIN, GitHub, and so on) |

Access evaluates the more specific path first. The agents' URL is then
`https://ingest.example.net/v1/ingest`, and everything else asks you to log
in. `curl` from your own machine can use a service token or `cloudflared
access curl`.

A frontend on another hostname (say `ninjagoldfinch.com`) calls the read API
cross-origin; list it in `cors_origins` and it gets the usual CORS headers.
Only `GET` and `OPTIONS` are allowed through, without credentials.

### Without cloudflared

Any TLS-terminating reverse proxy works instead, at the cost of an open
inbound port on your router and a DNS record for your home address. The
proxy must provide what Cloudflare Access was providing: a login on
everything except `/v1/ingest`, which the agents reach with their own
tokens. With Caddy, which fetches its own certificate:

```text
ingest.example.net {
    @agents path /v1/ingest
    reverse_proxy @agents localhost:9091

    basic_auth {
        ninja <hash from: caddy hash-password>
    }
    reverse_proxy localhost:9091
}
```

Both ends refuse an unencrypted path across the internet: the agent will
not push to a plain `http://` URL unless the host is loopback, and
ninja-ingest will not bind outside loopback unless told to.

## Configuration

One TOML file, `/etc/ninja-ingest/config.toml` by default (`--config
<path>`). It is checked at startup and the daemon refuses to start with a
clear message if anything is wrong, including unknown keys. Durations are
whole numbers with a unit: `ms`, `s`, `m`, `h` or `d`.
[`deploy/ingest.config.example.toml`](../deploy/ingest.config.example.toml)
is a full, commented example.

| Option | Default | Meaning |
|---|---|---|
| `[server] bind` | `127.0.0.1:9091` | Address for both the ingest route and the read API. |
| `[server] allow_non_loopback` | `false` | Must be `true` to bind anything other than loopback. |
| `[server] cors_origins` | `[]` | Browser origins allowed to call the read API. Empty means no CORS headers. |
| `[storage] path` | `/var/lib/ninja-ingest/ingest.db` | The SQLite file. The directory must exist. |
| `[storage] retention` | `30d` | Raw samples older than this are deleted, and samples that old are refused on arrival. At least `1h`. |
| `[storage] events_retention` | `180d` | Alert events older than this are deleted. |
| `[discord] enabled` | `false` | Post SILENT and REPORTING to the webhook in `NINJA_INGEST_DISCORD_WEBHOOK`. If the variable is missing, a warning is logged and the daemon runs without. |
| `[discord] startup_message` | `true` | Post `STARTED` at every start. |
| `[silence] after` | `2m` | No samples for this long means the host is silent. Keep it at least the agents' back-off cap (60s) plus two sampling intervals. |
| `[silence] repeat` | none | Remind on this interval while a host stays silent. |
| `[heartbeat] enabled` | `false` | GET a URL on a timer, but only after a database query answers, so a wedged server stops pinging. |
| `[heartbeat] url_env` | `NINJA_INGEST_HEARTBEAT_URL` | Environment variable holding the heartbeat URL. |
| `[heartbeat] interval` | `60s` | Time between heartbeats. |
| `[[agent]] name` | required | Must equal `[host] name` in that agent's config. |
| `[[agent]] token_env` | required | Environment variable holding that agent's bearer token. |

Environment variables, in `/etc/ninja-ingest/env`:

| Variable | Meaning |
|---|---|
| the `token_env` of each agent | That agent's bearer token. Secret. |
| `NINJA_INGEST_DISCORD_WEBHOOK` | Discord webhook URL. Secret. |
| `NINJA_INGEST_HEARTBEAT_URL` | Heartbeat URL (or whatever `url_env` names). Secret. |
| `NINJA_INGEST_LOG` | `error`, `warn`, `info` (default), `debug` or `trace`. |

Secrets are only ever read from the environment and never written to logs,
API responses or error messages.

## Tokens

Each agent has one bearer token, 32 random bytes as 64 hex characters.
`ninja-ingest --new-token` prints one; `install-ingest.sh --add-agent` calls
it for you. The server compares tokens in constant time and maps the match
to the agent's name, and the body's `host` must be that name, so a
misconfigured agent cannot write another host's history.

To rotate: `sudo ./install-ingest.sh --add-agent vps --start` here, then
`sudo ./install.sh --push --start` on the agent with the new token. The
agent buffers through the gap and logs a warning once a minute until the
new token is in place.

An agent whose variable is unset when the server starts is logged as a
warning and can never authenticate; the server still starts so the others
keep working.

## Who notices what

Once an agent pushes here, its own `[heartbeat]` is redundant.

| Failure | Noticed by | Message |
|---|---|---|
| VPS down, or its network | this server, within 2 to 2.5 minutes | `SILENT [vps]` |
| Agent crashed or stuck, VPS up | this server | `SILENT [vps]` |
| This server, the node, cloudflared or the home internet down | the agent, after `alert_after` (10 minutes) | `FIRING [vps] ingest unreachable` |
| Same, for longer than the agent's buffer | the agent; the gap shows in history | as above, plus an overflow warning in the agent's log |
| Both sides down at once | an external checker, via `[heartbeat]` here | whatever the checker sends |
| Discord down | nothing, until it is back; the queues retry | delayed messages |

Messages look like this:

```text
**STARTED** [ninja-ingest] ninja-ingest 0.2.0 is running, watching 2 hosts
**SILENT** [vps] no samples for 2m 10s, last seen 2026-10-11 14:02:11 UTC
**REPORTING** [vps] back after 12m 40s
```

With `repeat` set, `**SILENT** [vps] still no samples after 6h 2m, ...`
follows on that interval. Delivery uses the same bounded queue and retry
logic as the agent, and never pings anyone.

## API reference

All responses are JSON and all timestamps are Unix milliseconds (UTC).
Errors return `{"error": "..."}`. Only `POST /v1/ingest` needs a token; the
read routes rely on the proxy (Cloudflare Access) for access control.

### `POST /v1/ingest`

`Authorization: Bearer <token>`, JSON body:

```json
{
  "agent": "ninja-monitor/0.2.0",
  "host": "vps",
  "samples": [ ...the agent's /stats objects, oldest first... ],
  "events":  [ ...the agent's /alerts event objects... ]
}
```

| Status | Meaning |
|---|---|
| `200` `{"accepted": n, "duplicates": m, "rejected": r, "events_accepted": e}` | Stored. Duplicates (same host and timestamp) were ignored. `rejected` samples were outside the time window. |
| `400` | Malformed body, `host` does not match the token, timestamps not strictly increasing, a name over 64 bytes, or every sample rejected. |
| `401` | Missing or wrong token. |
| `413` | More than 200 samples or events, or a body over 2 MiB. |

Samples more than 5 minutes in the future or older than `retention` are
rejected. Unknown fields are ignored, so a newer agent can talk to an older
server.

### `GET /healthz`

```json
{"status":"ok","version":"0.2.0","uptime_secs":3600,"hosts":2,"db_bytes":52428800}
```

### `GET /hosts`

Every host that has reported, plus every configured agent that has not:

```json
{"hosts":[
  {"name":"home","status":"unknown","first_seen":null,"last_seen":null,"silent_since":null,"agent":null},
  {"name":"vps","status":"reporting","first_seen":1760000000000,"last_seen":1760003585000,
   "silent_since":null,"agent":"ninja-monitor/0.2.0"}
]}
```

`status` is `reporting`, `silent` or `unknown`. `last_seen` is this server's
clock at the last accepted push.

### `GET /hosts/{host}/stats`

The latest sample for that host, in the agent's `/stats` shape, plus
`received_at`. `404` for an unknown host, `503` for one with no samples yet.

### `GET /hosts/{host}/history`

| Parameter | Default | Meaning |
|---|---|---|
| `scope` | required | `host` or `service:<name>`. Unknown scopes return 404. |
| `minutes` | `60` | Window ending now. Capped at `retention`. |
| `from`, `to` | | Explicit window in Unix milliseconds, instead of `minutes`. Both together, at most `retention` apart. |
| `max_points` | `720` | Averaged into fixed-size buckets beyond this. Capped at 2000. |

The response has exactly the agent's `/history` shape: `scope`, `from`,
`to`, `minutes`, `bucket`, a shared `t` array and one array per field. A
test in this repository checks that the two are identical for the same
samples. A full 30 day window answers in about 130 ms.

### `GET /hosts/{host}/alerts?limit=100`

```json
{"events":[
  {"timestamp":1760003585000,"rule":"riot-proxy down","scope":"service:riot-proxy",
   "metric":"down","kind":"resolved","value":0.0,"op":null,"threshold":null,"held_secs":95}
]}
```

Newest first, `limit` capped at 1000. There is no `rules` array: the rules
live on the agent.

### `GET /stats`

One call for a dashboard: `{"hosts": [...]}` with each host's `/hosts`
summary plus `sample` (its latest, or `null`) and `received_at`.

## Storage and disk use

One SQLite file in WAL mode, with a table each for host samples, disk
samples, service samples and alert events, keyed by host and time. Writes
are one transaction per push with duplicates ignored. A sweeper deletes rows
past their retention an hour after start and every six hours after that, in
batches of 10 000 so the write lock is never held for long, then truncates
the WAL. Free pages are reused; `VACUUM` is never run.

Size: about 450 bytes per sample row set, so roughly

```
MiB per host per 30 days ≈ 1.2 × (1 + mounts + services) × (3600 / interval_seconds) × 24 × 30 / 1024
```

Measured: one host with one mount and two services at 15 second samples
took 51 MiB for 30 days; three services would be about 65 MiB.

## Backup and uninstall

The database is a single file. While the service is running, take a
consistent copy with SQLite's own backup:

```sh
sudo -u ninja-ingest sqlite3 /var/lib/ninja-ingest/ingest.db ".backup /tmp/ingest-backup.db"
```

Or stop the service and copy the file. Restoring is the reverse: stop, put
the file back, start. The schema is versioned; a newer binary migrates an
older file at start, and an older binary refuses a newer file with a clear
message.

`sudo ./install-ingest.sh --uninstall` removes the service, binary and user
and keeps `/etc/ninja-ingest` and `/var/lib/ninja-ingest`.

## Development

Everything builds and tests on macOS, from the repository root:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Run a local server and point a local agent at it:

```sh
mkdir -p /tmp/ninja-ingest
cat > /tmp/ingest.toml <<EOF
storage.path = "/tmp/ninja-ingest/ingest.db"
[[agent]]
name = "dev"
token_env = "NINJA_INGEST_TOKEN_DEV"
EOF
NINJA_INGEST_TOKEN_DEV=devtoken-devtoken-devtoken-devtoken cargo run -p ninja-ingest -- --config /tmp/ingest.toml
```

Then in the agent's `dev/config.toml` enable the commented `[push]` block
and run it with `NINJA_MONITOR_PUSH_TOKEN=devtoken-devtoken-devtoken-devtoken`.
`curl -s localhost:9091/hosts` shows it reporting.

The ignored test `full_retention_query_timing` fills 30 days of samples and
times the widest query:

```sh
cargo test -p ninja-ingest --release --test api -- --ignored --nocapture
```
