#!/bin/sh
# Installs, upgrades, configures or removes ninja-monitor. Safe to run
# repeatedly: it never overwrites an existing config or env file unless asked,
# and only starts the service when given --start.
#
# Usage (run from a clone of the repository after `cargo build --release`):
#   sudo ./deploy/install.sh [options] [path/to/ninja-monitor]
#
# Options:
#   --start        enable the service and (re)start it
#   --webhook      prompt for the Discord webhook URL and save it
#   --heartbeat    prompt for a heartbeat URL, save it and turn the heartbeat on
#   --push         prompt for a ninja-ingest URL and token, save them and turn pushing on
#   --reconfigure  regenerate the config from running services (old one kept as .bak)
#   --uninstall    stop and remove the service, binary and user (keeps /etc/ninja-monitor)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
etc=/etc/ninja-monitor
user=ninja-monitor
installed=/usr/local/bin/ninja-monitor
unit=/etc/systemd/system/ninja-monitor.service

start=no webhook=no heartbeat=no push=no reconfigure=no uninstall=no bin=
for arg in "$@"; do
    case $arg in
        --start) start=yes ;;
        --webhook) webhook=yes ;;
        --heartbeat) heartbeat=yes ;;
        --push) push=yes ;;
        --reconfigure) reconfigure=yes ;;
        --uninstall) uninstall=yes ;;
        -h | --help)
            sed -n '2,16s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
        -*)
            echo "install.sh: unknown option $arg (try --help)" >&2
            exit 2
            ;;
        *) bin=$arg ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh: run as root (sudo $0 $*)" >&2
    exit 1
fi

have_systemd() { [ -d /run/systemd/system ]; }

if [ "$uninstall" = yes ]; then
    if have_systemd; then
        systemctl disable --now ninja-monitor 2>/dev/null || true
    fi
    rm -f "$unit" "$installed"
    if have_systemd; then
        systemctl daemon-reload
    fi
    if id "$user" >/dev/null 2>&1; then
        userdel "$user"
    fi
    echo "removed ninja-monitor; $etc was kept (delete it with: sudo rm -r $etc)"
    exit 0
fi

# The binary: an explicit path, one next to this script, or the build output.
if [ -z "$bin" ]; then
    for candidate in "$here/ninja-monitor" "$here/../target/release/ninja-monitor"; do
        if [ -f "$candidate" ]; then
            bin=$candidate
            break
        fi
    done
fi
if [ -z "$bin" ] && [ ! -x "$installed" ]; then
    echo "install.sh: no binary found; run \`cargo build --release\` first or pass its path" >&2
    exit 1
fi
for f in "$here/ninja-monitor.service" "$here/env.example"; do
    if [ ! -f "$f" ]; then
        echo "install.sh: missing $f" >&2
        exit 1
    fi
done

if ! id "$user" >/dev/null 2>&1; then
    useradd --system --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin --user-group "$user"
    echo "created system user $user"
fi

if [ -n "$bin" ]; then
    install -m 0755 -o root -g root "$bin" "$installed.new"
    mv -f "$installed.new" "$installed"
    echo "installed $installed ($("$installed" --version))"
fi

install -d -m 0750 -o root -g "$user" "$etc"

if [ "$reconfigure" = yes ] && [ -e "$etc/config.toml" ]; then
    cp -p "$etc/config.toml" "$etc/config.toml.bak"
    echo "backed up the old config to $etc/config.toml.bak"
fi
if [ ! -e "$etc/config.toml" ] || [ "$reconfigure" = yes ]; then
    generated=$(mktemp)
    "$installed" --init > "$generated"
    install -m 0640 -o root -g "$user" "$generated" "$etc/config.toml"
    rm -f "$generated"
    echo "wrote $etc/config.toml for these running services:"
    sed -n 's/^unit = "\(.*\)"$/  \1/p' "$etc/config.toml"
    # Keep the heartbeat on if a URL was already saved.
    if grep -q '^NINJA_MONITOR_HEARTBEAT_URL=.' "$etc/env" 2>/dev/null; then
        sed -i '/^\[heartbeat\]/,/^\[/ s/^enabled = false/enabled = true/' "$etc/config.toml"
    fi
    # Keep pushing on if it was set up before (the URL lives in the old config).
    if [ -f "$etc/config.toml.bak" ]; then
        old_url=$(awk -F'"' '/^\[/ { s = ($0 ~ /^\[push\]/) } s && /^url = / { print $2 }' "$etc/config.toml.bak")
        if [ -n "$old_url" ]; then
            printf '\n[push]\nenabled = true\nurl = "%s"\n' "$old_url" >> "$etc/config.toml"
            echo "kept pushing to $old_url"
        fi
    fi
else
    echo "kept existing $etc/config.toml"
fi

if [ ! -e "$etc/env" ]; then
    install -m 0600 -o root -g root "$here/env.example" "$etc/env"
    echo "created $etc/env (mode 0600)"
fi

# Reads one line from the terminal without echoing it.
prompt_secret() {
    printf '%s' "$1" > /dev/tty
    stty -echo < /dev/tty
    trap 'stty echo < /dev/tty' EXIT INT TERM
    IFS= read -r secret < /dev/tty || secret=
    stty echo < /dev/tty
    trap - EXIT INT TERM
    printf '\n' > /dev/tty
    secret=$(printf '%s' "$secret" | tr -d '[:space:]')
}

# Sets KEY=VALUE in the env file, replacing any existing line for KEY.
set_env() {
    tmp=$(mktemp "$etc/.env.XXXXXX")
    KEY=$1 VALUE=$2 awk '
        BEGIN { key = ENVIRON["KEY"]; line = key "=" ENVIRON["VALUE"] }
        index($0, key "=") == 1 { if (!done) print line; done = 1; next }
        { print }
        END { if (!done) print line }
    ' "$etc/env" > "$tmp"
    chown root:root "$tmp"
    chmod 0600 "$tmp"
    mv -f "$tmp" "$etc/env"
}

if [ "$webhook" = yes ]; then
    prompt_secret "Discord webhook URL (input hidden): "
    case $secret in
        https://*) set_env NINJA_MONITOR_DISCORD_WEBHOOK "$secret" && echo "saved the webhook to $etc/env" ;;
        *) echo "install.sh: that does not look like an https:// URL; nothing saved" >&2 ;;
    esac
fi

if [ "$heartbeat" = yes ]; then
    url_env=$(awk -F'"' '/^\[/ { s = ($0 ~ /^\[heartbeat\]/) } s && /^url_env/ { print $2 }' "$etc/config.toml")
    url_env=${url_env:-NINJA_MONITOR_HEARTBEAT_URL}
    prompt_secret "Heartbeat URL (input hidden): "
    case $secret in
        http://* | https://*)
            set_env "$url_env" "$secret"
            sed -i '/^\[heartbeat\]/,/^\[/ s/^enabled = false/enabled = true/' "$etc/config.toml"
            echo "saved the heartbeat URL to $etc/env and enabled the heartbeat"
            ;;
        *) echo "install.sh: that does not look like a URL; nothing saved" >&2 ;;
    esac
fi

if [ "$push" = yes ]; then
    printf 'ninja-ingest URL (for example https://ingest.example.net/v1/ingest): ' > /dev/tty
    IFS= read -r url < /dev/tty || url=
    url=$(printf '%s' "$url" | tr -d '[:space:]')
    case $url in
        https://* | http://*) ;;
        *)
            echo "install.sh: that does not look like a URL; nothing saved" >&2
            url=
            ;;
    esac
    if [ -n "$url" ]; then
        prompt_secret "Agent token from install-ingest.sh --add-agent (input hidden): "
        if [ -z "$secret" ]; then
            echo "install.sh: no token given; nothing saved" >&2
        else
            set_env NINJA_MONITOR_PUSH_TOKEN "$secret"
            if grep -q '^\[push\]' "$etc/config.toml"; then
                URL=$url awk '
                    /^\[/ { s = ($0 ~ /^\[push\]/) }
                    s && /^enabled = / { print "enabled = true"; next }
                    s && /^url = / { print "url = \"" ENVIRON["URL"] "\""; next }
                    { print }
                ' "$etc/config.toml" > "$etc/.config.new"
                chown root:"$user" "$etc/.config.new"
                chmod 0640 "$etc/.config.new"
                mv -f "$etc/.config.new" "$etc/config.toml"
            else
                printf '\n[push]\nenabled = true\nurl = "%s"\n' "$url" >> "$etc/config.toml"
            fi
            echo "saved the token to $etc/env and enabled pushing to $url"
        fi
    fi
fi

# The unit is always refreshed; keep local changes in a drop-in
# (sudo systemctl edit ninja-monitor) so upgrades preserve them.
install -m 0644 -o root -g root "$here/ninja-monitor.service" "$unit"
if have_systemd; then
    systemctl daemon-reload
fi

# Check the config the way the service will see it: as its user, with the env file loaded.
echo
echo "Checking the setup as the $user user:"
if report=$(
    while IFS= read -r line; do
        case $line in
            ('' | '#'*) ;;
            (*=*) export "${line?}" ;;
        esac
    done < "$etc/env"
    runuser -u "$user" -- "$installed" --check --config "$etc/config.toml" 2>&1
); then
    printf '%s\n' "$report" | sed 's/^/  /'
else
    printf '%s\n' "$report" | sed 's/^/  /' >&2
    echo "install.sh: fix the config above (sudoedit $etc/config.toml) and run this again" >&2
    exit 1
fi

if [ "$start" = yes ]; then
    if ! have_systemd; then
        echo "install.sh: systemd is not running here, cannot start the service" >&2
        exit 1
    fi
    systemctl enable --quiet ninja-monitor
    systemctl restart ninja-monitor
    sleep 2
    if systemctl is-active --quiet ninja-monitor; then
        echo
        echo "ninja-monitor is running. Recent log:"
        journalctl -u ninja-monitor -n 5 --no-pager -o cat | sed 's/^/  /'
    else
        echo
        echo "ninja-monitor failed to start. Recent log:" >&2
        journalctl -u ninja-monitor -n 30 --no-pager -o cat >&2
        exit 1
    fi
elif have_systemd && systemctl is-active --quiet ninja-monitor; then
    echo
    echo "ninja-monitor keeps its old binary, config and secrets until restarted:"
    echo "  sudo systemctl restart ninja-monitor   (or re-run with --start)"
else
    echo
    echo "Not started. When ready: sudo $0 --start"
fi
