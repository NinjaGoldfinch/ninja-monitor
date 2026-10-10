# Test fixtures

Fake `proc` and cgroup v2 trees used by the tests and by dev mode.

- `proc/` and `cgroup/system.slice/cloudflared.service/` are copied from the
  target VPS (Ubuntu 26.04, kernel 7.0, 2 cores, no swap).
- `riot-proxy.service` uses the same layout with values taken from that
  service's real RSS split (about 200 MiB anon, 390 MiB file-backed). It has no
  `memory.peak`, to cover kernels that lack the file.
- `site.service` has no directory at all, so it reports as down.
