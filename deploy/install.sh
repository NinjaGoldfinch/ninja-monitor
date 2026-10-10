#!/bin/sh
# Installs or upgrades ninja-monitor. Safe to run repeatedly. It never
# overwrites an existing config or env file and never enables or starts
# the service.
#
# Usage, from a directory holding the binary and the deploy/ files:
#   sudo ./install.sh [path/to/ninja-monitor]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-"$here/ninja-monitor"}
etc=/etc/ninja-monitor
user=ninja-monitor

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh: run as root (sudo ./install.sh)" >&2
    exit 1
fi
for f in "$bin" "$here/ninja-monitor.service" "$here/config.example.toml" "$here/env.example"; do
    if [ ! -f "$f" ]; then
        echo "install.sh: missing $f" >&2
        exit 1
    fi
done

if id "$user" >/dev/null 2>&1; then
    echo "user $user already exists"
else
    useradd --system --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin --user-group "$user"
    echo "created system user $user"
fi

install -m 0755 -o root -g root "$bin" /usr/local/bin/ninja-monitor
echo "installed /usr/local/bin/ninja-monitor"

install -d -m 0750 -o root -g "$user" "$etc"

if [ -e "$etc/config.toml" ]; then
    echo "kept existing $etc/config.toml"
else
    install -m 0640 -o root -g "$user" "$here/config.example.toml" "$etc/config.toml"
    echo "installed $etc/config.toml from the example; edit it before starting"
fi

if [ -e "$etc/env" ]; then
    echo "kept existing $etc/env"
else
    install -m 0600 -o root -g root "$here/env.example" "$etc/env"
    echo "installed $etc/env (mode 0600); add your webhook URL to it"
fi

# The unit is always refreshed; put local changes in a drop-in
# (systemctl edit ninja-monitor) so upgrades keep them.
install -m 0644 -o root -g root "$here/ninja-monitor.service" /etc/systemd/system/ninja-monitor.service
echo "installed /etc/systemd/system/ninja-monitor.service"

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    echo "reloaded systemd"
else
    echo "systemd is not running here; skipped daemon-reload"
fi

cat <<EOF

Next steps:
  1. Edit $etc/config.toml (unit names and paths are placeholders).
  2. Put secrets in $etc/env.
  3. Check the config:  sudo -u $user /usr/local/bin/ninja-monitor --config $etc/config.toml --check
  4. Start it:          sudo systemctl enable --now ninja-monitor
  5. Watch the log:     journalctl -u ninja-monitor -f
EOF
