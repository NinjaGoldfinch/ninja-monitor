#!/bin/sh
# Installs, upgrades, configures or removes ninja-ingest. Safe to run
# repeatedly: it never overwrites an existing config or env file unless asked,
# and only starts the service when given --start.
#
# Usage (from the extracted release, or a clone after `cargo build --release`):
#   sudo ./install-ingest.sh [options] [path/to/ninja-ingest]
#
# Options:
#   --start             enable the service and (re)start it
#   --add-agent <name>  add an agent (or rotate its token) and print the token once
#   --webhook           prompt for the Discord webhook URL, save it and turn Discord on
#   --heartbeat         prompt for a heartbeat URL, save it and turn the heartbeat on
#   --uninstall         stop and remove the service, binary and user
#                       (keeps /etc/ninja-ingest and /var/lib/ninja-ingest)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
etc=/etc/ninja-ingest
state=/var/lib/ninja-ingest
user=ninja-ingest
installed=/usr/local/bin/ninja-ingest
unit=/etc/systemd/system/ninja-ingest.service

start=no webhook=no heartbeat=no uninstall=no agent='' bin=''
while [ $# -gt 0 ]; do
    case $1 in
        --start) start=yes ;;
        --webhook) webhook=yes ;;
        --heartbeat) heartbeat=yes ;;
        --uninstall) uninstall=yes ;;
        --add-agent)
            shift
            agent=${1:-}
            if [ -z "$agent" ]; then
                echo "install-ingest.sh: --add-agent needs a name" >&2
                exit 2
            fi
            ;;
        --add-agent=*) agent=${1#--add-agent=} ;;
        -h | --help)
            sed -n '2,16s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
        -*)
            echo "install-ingest.sh: unknown option $1 (try --help)" >&2
            exit 2
            ;;
        *) bin=$1 ;;
    esac
    shift
done

if [ "$(id -u)" -ne 0 ]; then
    echo "install-ingest.sh: run as root (sudo $0 $*)" >&2
    exit 1
fi

have_systemd() { [ -d /run/systemd/system ]; }

if [ "$uninstall" = yes ]; then
    if have_systemd; then
        systemctl disable --now ninja-ingest 2>/dev/null || true
    fi
    rm -f "$unit" "$installed"
    if have_systemd; then
        systemctl daemon-reload
    fi
    if id "$user" >/dev/null 2>&1; then
        userdel "$user"
    fi
    echo "removed ninja-ingest; $etc and $state were kept"
    echo "  (delete them with: sudo rm -r $etc $state)"
    exit 0
fi

# The binary: an explicit path, one next to this script, or the build output.
if [ -z "$bin" ]; then
    for candidate in "$here/ninja-ingest" "$here/../target/release/ninja-ingest"; do
        if [ -f "$candidate" ]; then
            bin=$candidate
            break
        fi
    done
fi
if [ -z "$bin" ] && [ ! -x "$installed" ]; then
    echo "install-ingest.sh: no binary found; run \`cargo build --release\` first or pass its path" >&2
    exit 1
fi
for f in "$here/ninja-ingest.service" "$here/ingest.env.example" "$here/ingest.config.example.toml"; do
    if [ ! -f "$f" ]; then
        echo "install-ingest.sh: missing $f" >&2
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
# systemd would create this at first start; creating it now lets --check
# report on it before then.
install -d -m 0750 -o "$user" -g "$user" "$state"

if [ ! -e "$etc/config.toml" ]; then
    install -m 0640 -o root -g "$user" "$here/ingest.config.example.toml" "$etc/config.toml"
    echo "wrote $etc/config.toml (no agents yet; add one with --add-agent <name>)"
else
    echo "kept existing $etc/config.toml"
fi

if [ ! -e "$etc/env" ]; then
    install -m 0600 -o root -g root "$here/ingest.env.example" "$etc/env"
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
        index($0, "#" key "=") == 1 { if (!done) print line; done = 1; next }
        { print }
        END { if (!done) print line }
    ' "$etc/env" > "$tmp"
    chown root:root "$tmp"
    chmod 0600 "$tmp"
    mv -f "$tmp" "$etc/env"
}

# Flips `enabled = false` to true inside one [section] of the config.
enable_section() {
    sed -i "/^\[$1\]/,/^\[/ s/^enabled = false/enabled = true/" "$etc/config.toml"
}

if [ -n "$agent" ]; then
    case $agent in
        *[!A-Za-z0-9._-]* | '')
            echo "install-ingest.sh: agent names use letters, digits, dot, dash and underscore" >&2
            exit 2
            ;;
    esac
    var="NINJA_INGEST_TOKEN_$(printf '%s' "$agent" | tr 'a-z.-' 'A-Z__')"
    token=$("$installed" --new-token)
    if grep -q "^name = \"$agent\"$" "$etc/config.toml"; then
        # Already configured: this is a token rotation. Keep whatever
        # token_env the block names.
        var=$(awk -v name="$agent" -F'"' '
            /^\[\[agent\]\]/ { block = 1; match_name = 0; next }
            block && /^name = / { match_name = ($2 == name) }
            block && match_name && /^token_env = / { print $2; exit }
        ' "$etc/config.toml")
        var=${var:-NINJA_INGEST_TOKEN_$(printf '%s' "$agent" | tr 'a-z.-' 'A-Z__')}
        set_env "$var" "$token"
        echo "rotated the token for agent $agent ($var)"
    else
        printf '\n[[agent]]\nname = "%s"\ntoken_env = "%s"\n' "$agent" "$var" >> "$etc/config.toml"
        set_env "$var" "$token"
        echo "added agent $agent to $etc/config.toml ($var in $etc/env)"
    fi
    echo
    echo "Token for $agent (shown once; it is saved in $etc/env):"
    echo
    echo "  $token"
    echo
    echo "On that machine, run: sudo ./install.sh --push --start"
    echo "and paste this server's ingest URL and the token when asked."
    echo "Then restart here with --start so the new token is loaded."
fi

if [ "$webhook" = yes ]; then
    prompt_secret "Discord webhook URL (input hidden): "
    case $secret in
        https://*)
            set_env NINJA_INGEST_DISCORD_WEBHOOK "$secret"
            enable_section discord
            echo "saved the webhook to $etc/env and enabled Discord"
            ;;
        *) echo "install-ingest.sh: that does not look like an https:// URL; nothing saved" >&2 ;;
    esac
fi

if [ "$heartbeat" = yes ]; then
    url_env=$(awk -F'"' '/^\[/ { s = ($0 ~ /^\[heartbeat\]/) } s && /^url_env/ { print $2 }' "$etc/config.toml")
    url_env=${url_env:-NINJA_INGEST_HEARTBEAT_URL}
    prompt_secret "Heartbeat URL (input hidden): "
    case $secret in
        http://* | https://*)
            set_env "$url_env" "$secret"
            enable_section heartbeat
            echo "saved the heartbeat URL to $etc/env and enabled the heartbeat"
            ;;
        *) echo "install-ingest.sh: that does not look like a URL; nothing saved" >&2 ;;
    esac
fi

# The unit is always refreshed; keep local changes in a drop-in
# (sudo systemctl edit ninja-ingest) so upgrades preserve them.
install -m 0644 -o root -g root "$here/ninja-ingest.service" "$unit"
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
    echo "install-ingest.sh: fix the config above (sudoedit $etc/config.toml) and run this again" >&2
    exit 1
fi

if [ "$start" = yes ]; then
    if ! have_systemd; then
        echo "install-ingest.sh: systemd is not running here, cannot start the service" >&2
        exit 1
    fi
    systemctl enable --quiet ninja-ingest
    systemctl restart ninja-ingest
    sleep 2
    if systemctl is-active --quiet ninja-ingest; then
        echo
        echo "ninja-ingest is running. Recent log:"
        journalctl -u ninja-ingest -n 5 --no-pager -o cat | sed 's/^/  /'
    else
        echo
        echo "ninja-ingest failed to start. Recent log:" >&2
        journalctl -u ninja-ingest -n 30 --no-pager -o cat >&2
        exit 1
    fi
elif have_systemd && systemctl is-active --quiet ninja-ingest; then
    echo
    echo "ninja-ingest keeps its old binary, config and secrets until restarted:"
    echo "  sudo systemctl restart ninja-ingest   (or re-run with --start)"
else
    echo
    echo "Not started. When ready: sudo $0 --start"
fi
