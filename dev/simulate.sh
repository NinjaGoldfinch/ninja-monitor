#!/usr/bin/env bash
# Copies the fixture trees to dev/live and keeps changing their values so the
# dev API shows moving data. Usage: dev/simulate.sh [seconds-between-updates]
set -euo pipefail

cd "$(dirname "$0")/.."
step=${1:-5}
live=dev/live
rp=$live/cgroup/system.slice/riot-proxy.service
cf=$live/cgroup/system.slice/cloudflared.service

rm -rf "$live"
mkdir -p "$live"
cp -R tests/fixtures/proc tests/fixtures/cgroup "$live"/

# Rewrite through a temp file so the monitor never reads a half-written file.
put() { printf '%s\n' "$2" > "$1.tmp" && mv "$1.tmp" "$1"; }
edit() { local file=$1; shift; awk "$@" "$file" > "$file.tmp" && mv "$file.tmp" "$file"; }
rand() { echo $(( $1 + RANDOM % ($2 - $1 + 1) )); }

uptime=18486
echo "simulating in $live every ${step}s, Ctrl-C to stop"
while true; do
    sleep "$step"
    uptime=$(( uptime + step ))

    # Host: 2 cores at USER_HZ 100, busy between 5 and 40 percent.
    total=$(( step * 200 ))
    busy=$(( total * $(rand 5 40) / 100 ))
    edit "$live/proc/stat" -v b="$busy" -v i="$(( total - busy ))" \
        '$1 == "cpu" { $2 += b; $5 += i } { print }'
    edit "$live/proc/meminfo" -v kb="$(rand 2900000 3200000)" \
        '$1 == "MemAvailable:" { $2 = kb } { print }'
    put "$live/proc/loadavg" "0.$(rand 10 90) 0.$(rand 20 50) 0.$(rand 20 40) 2/177 5782"
    put "$live/proc/uptime" "$uptime.00 30556.41"

    # riot-proxy: busy and growing; cloudflared: nearly idle.
    edit "$rp/cpu.stat" -v us="$(( step * 10000 * $(rand 10 60) ))" \
        '$1 == "usage_usec" { $2 += us } { print }'
    anon=$(( 208506880 + $(rand 0 50) * 1048576 ))
    file=$(( 403083264 + $(rand 0 20) * 1048576 ))
    edit "$rp/memory.stat" -v a="$anon" -v f="$file" \
        '$1 == "anon" { $2 = a } $1 == "file" { $2 = f } { print }'
    put "$rp/memory.current" "$(( anon + file + 6807552 ))"
    put "$rp/pids.current" "$(rand 12 16)"

    edit "$cf/cpu.stat" -v us="$(( step * 10000 * $(rand 0 2) ))" \
        '$1 == "usage_usec" { $2 += us } { print }'
done
