# ninja-monitor

A small Rust daemon that watches one Linux VPS. It samples CPU, memory and disk
usage for the host and for each configured systemd service (via cgroup v2),
keeps recent history in memory, serves it as JSON on localhost, sends alerts to
a Discord webhook and can ping an external heartbeat checker.

It aims to be the lightest thing on the box: one process, a current-thread
async runtime, no database and nothing written to disk.

> Full install and configuration docs are on the way. Until then, see
> [`dev/config.toml`](dev/config.toml) for a commented example config.

## API

All endpoints are bound to loopback only and return JSON. Timestamps are Unix
milliseconds.

| Route | Returns |
|---|---|
| `GET /healthz` | Status, version and process uptime |
| `GET /stats` | The latest sample: `host` plus a `services` array |
| `GET /history?scope=host&minutes=60` | Host history as columns sharing a `t` array |
| `GET /history?scope=service:<name>&minutes=60&max_points=720` | One service's history, averaged into buckets when needed |
| `GET /alerts` | Each rule's state and the last 100 alert events |

## Memory: anon versus file

A service's `mem_current_bytes` includes page cache, which the kernel reclaims
on demand. A service with a large memory-mapped database can look huge by that
number while its real footprint is small. `mem_anon_bytes` is the service's own
memory and is the figure to watch and alert on.

## Development

Everything builds and tests on macOS against fixture trees in
[`tests/fixtures`](tests/fixtures).

```sh
cargo test
dev/simulate.sh                          # terminal 1: keeps fixture values moving
cargo run -- --config dev/config.toml    # terminal 2
curl -s localhost:9090/stats             # terminal 3
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
