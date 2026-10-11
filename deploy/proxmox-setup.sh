#!/bin/sh
# Creates an unprivileged Debian LXC container on a Proxmox host and sets up
# ninja-ingest inside it: the release binary, the service, your agents'
# tokens, the Discord webhook and a cloudflared tunnel. Run it once, as root,
# on the Proxmox host.
#
# Usage:
#   ./proxmox-setup.sh [options] --agent <name> [--agent <name> ...]
#
# Options:
#   --agent <name>        a host that will push here (its [host] name); repeatable
#   --id <ctid>           container id (default: next free id)
#   --hostname <name>     container hostname (default: ninja-ingest)
#   --storage <name>      storage for the root disk (default: local-lvm)
#   --template-storage <name>  storage for the Debian template (default: local)
#   --bridge <name>       network bridge (default: vmbr0)
#   --ip <cidr>,gw=<ip>   static address, for example 192.168.1.50/24,gw=192.168.1.1
#                         (default: dhcp)
#   --memory <MiB>        memory limit (default: 512)
#   --disk <GiB>          root disk size (default: 8)
#   --release <tag>       ninja-ingest release to install (default: latest)
#   --no-discord          do not ask for a Discord webhook
#   --no-cloudflared      do not install cloudflared (you run a tunnel or proxy elsewhere)
#   --expose              listen on the container's address instead of loopback, for a
#                         cloudflared or reverse proxy in another container; implies
#                         --no-cloudflared
#
# Secrets (webhook, tunnel token) are typed at hidden prompts and reach the
# container through a root-only file, never through a command line.
# The single-quoted blocks below are scripts that run inside the container;
# their variables are meant to expand there, not here.
# shellcheck disable=SC2016
set -eu

agents='' ctid='' hostname=ninja-ingest storage=local-lvm tstorage=local bridge=vmbr0
ip=dhcp memory=512 disk=8 release=latest discord=yes cloudflared=yes expose=no
while [ $# -gt 0 ]; do
    case $1 in
        --agent) shift; agents="$agents ${1:-}" ;;
        --id) shift; ctid=${1:-} ;;
        --hostname) shift; hostname=${1:-} ;;
        --storage) shift; storage=${1:-} ;;
        --template-storage) shift; tstorage=${1:-} ;;
        --bridge) shift; bridge=${1:-} ;;
        --ip) shift; ip=${1:-} ;;
        --memory) shift; memory=${1:-} ;;
        --disk) shift; disk=${1:-} ;;
        --release) shift; release=${1:-} ;;
        --no-discord) discord=no ;;
        --no-cloudflared) cloudflared=no ;;
        --expose) expose=yes; cloudflared=no ;;
        -h | --help)
            sed -n '2,30s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
        *)
            echo "proxmox-setup.sh: unknown option $1 (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

if [ "$(id -u)" -ne 0 ]; then
    echo "proxmox-setup.sh: run as root on the Proxmox host" >&2
    exit 1
fi
if ! command -v pct >/dev/null 2>&1; then
    echo "proxmox-setup.sh: pct not found; this runs on a Proxmox VE host" >&2
    exit 1
fi
if [ -z "$agents" ]; then
    echo "proxmox-setup.sh: name at least one agent with --agent <name> (you can add more later" >&2
    echo "  with: pct exec <ctid> -- /root/ninja-ingest/install-ingest.sh --add-agent <name> --start)" >&2
    exit 2
fi
for name in $agents; do
    case $name in
        *[!A-Za-z0-9._-]* | '')
            echo "proxmox-setup.sh: agent names use letters, digits, dot, dash and underscore" >&2
            exit 2
            ;;
    esac
done
case $release in
    latest) release_path=latest/download ;;
    v*) release_path="download/$release" ;;
    *) release_path="download/v$release" ;;
esac

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

# Ask for everything up front, so the rest runs unattended.
webhook='' tunnel_token=''
if [ "$discord" = yes ]; then
    prompt_secret "Discord webhook URL (input hidden, empty to skip): "
    case $secret in
        https://*) webhook=$secret ;;
        '') echo "no webhook; Discord stays off (enable later with install-ingest.sh --webhook)" ;;
        *) echo "proxmox-setup.sh: that does not look like an https:// URL" >&2; exit 2 ;;
    esac
fi
if [ "$cloudflared" = yes ]; then
    echo "In Cloudflare Zero Trust, create a tunnel (Networks > Tunnels > Create, 'cloudflared')"
    echo "and copy the token from the install command it shows (the long string after 'install')."
    prompt_secret "Tunnel token (input hidden, empty to skip cloudflared): "
    if [ -n "$secret" ]; then
        tunnel_token=$secret
    else
        cloudflared=no
        echo "no token; cloudflared not installed (see the README to add it later)"
    fi
fi

# --- the container ------------------------------------------------------------
if [ -z "$ctid" ]; then
    ctid=$(pvesh get /cluster/nextid)
fi
if pct status "$ctid" >/dev/null 2>&1; then
    echo "proxmox-setup.sh: container $ctid already exists; pick another with --id" >&2
    exit 1
fi

echo "Finding the newest Debian 12 template..."
pveam update >/dev/null
template=$(pveam available --section system | awk '$2 ~ /^debian-12-standard_/ { print $2 }' | sort -V | tail -n 1)
if [ -z "$template" ]; then
    echo "proxmox-setup.sh: no debian-12-standard template offered by pveam" >&2
    exit 1
fi
if ! pveam list "$tstorage" | grep -q "$template"; then
    echo "Downloading $template to $tstorage..."
    pveam download "$tstorage" "$template" >/dev/null
fi

echo "Creating container $ctid ($hostname) on $storage..."
# nesting=1 lets systemd set up the mount namespaces the hardened unit asks
# for inside an unprivileged container.
pct create "$ctid" "$tstorage:vztmpl/$template" \
    --hostname "$hostname" \
    --ostype debian \
    --unprivileged 1 \
    --features nesting=1 \
    --cores 1 \
    --memory "$memory" \
    --swap 0 \
    --rootfs "$storage:$disk" \
    --net0 "name=eth0,bridge=$bridge,ip=$ip" \
    --onboot 1 \
    --start 1 >/dev/null

echo "Waiting for the container's network..."
tries=0
until pct exec "$ctid" -- sh -c 'getent hosts deb.debian.org >/dev/null 2>&1'; do
    tries=$((tries + 1))
    if [ "$tries" -ge 30 ]; then
        echo "proxmox-setup.sh: container $ctid has no network after 60s; check --bridge and --ip" >&2
        exit 1
    fi
    sleep 2
done

# Runs a script inside the container with stdin closed, failing loudly.
in_ct() {
    pct exec "$ctid" -- sh -c "$1" < /dev/null
}

echo "Installing ninja-ingest ($release) inside the container..."
in_ct "
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq curl ca-certificates >/dev/null
arch=\$(uname -m)
base=https://github.com/NinjaGoldfinch/ninja-monitor/releases/$release_path
cd /root
rm -rf ninja-ingest
curl -fsSLO \"\$base/ninja-ingest-\$arch-linux.tar.gz\"
curl -fsSLO \"\$base/SHA256SUMS\"
sha256sum --ignore-missing -c SHA256SUMS >/dev/null
tar -xzf \"ninja-ingest-\$arch-linux.tar.gz\"
rm -f \"ninja-ingest-\$arch-linux.tar.gz\" SHA256SUMS
cd ninja-ingest
./install-ingest.sh >/dev/null
"
if [ "$expose" = yes ]; then
    echo "Binding ninja-ingest on all of the container's addresses (--expose)..."
    in_ct '
set -eu
cfg=/etc/ninja-ingest/config.toml
sed -i "/^\[server\]/,/^\[/ { s/^bind = .*/bind = \"0.0.0.0:9091\"/; s/^allow_non_loopback = false/allow_non_loopback = true/; }" "$cfg"
grep -q "^allow_non_loopback = true" "$cfg"
'
fi

# Agents: the token lines are the indented 64-hex lines the installer prints.
tokens=''
for name in $agents; do
    out=$(in_ct "cd /root/ninja-ingest && ./install-ingest.sh --add-agent '$name'")
    token=$(printf '%s\n' "$out" | awk '/^  [0-9a-f]{64}$/ { print $1 }' | tail -n 1)
    if [ -z "$token" ]; then
        printf '%s\n' "$out" >&2
        echo "proxmox-setup.sh: could not read the token for $name from the installer's output" >&2
        exit 1
    fi
    tokens="$tokens
  $name: $token"
done

# Secrets travel in a root-only file, merged into the env file inside.
if [ -n "$webhook" ] || [ -n "$tunnel_token" ]; then
    secrets=$(mktemp)
    chmod 0600 "$secrets"
    {
        [ -n "$webhook" ] && echo "NINJA_INGEST_DISCORD_WEBHOOK=$webhook"
        [ -n "$tunnel_token" ] && echo "TUNNEL_TOKEN=$tunnel_token"
    } > "$secrets"
    pct push "$ctid" "$secrets" /root/.ninja-secrets --perms 0600 --user 0 --group 0
    rm -f "$secrets"
    in_ct '
set -eu
etc=/etc/ninja-ingest
while IFS= read -r line; do
    key=${line%%=*}
    value=${line#*=}
    case $key in
        NINJA_INGEST_DISCORD_WEBHOOK)
            tmp=$(mktemp "$etc/.env.XXXXXX")
            KEY=$key VALUE=$value awk '"'"'
                BEGIN { key = ENVIRON["KEY"]; line = key "=" ENVIRON["VALUE"] }
                index($0, key "=") == 1 { if (!done) print line; done = 1; next }
                index($0, "#" key "=") == 1 { if (!done) print line; done = 1; next }
                { print }
                END { if (!done) print line }
            '"'"' "$etc/env" > "$tmp"
            chown root:root "$tmp"; chmod 0600 "$tmp"; mv -f "$tmp" "$etc/env"
            sed -i "/^\[discord\]/,/^\[/ s/^enabled = false/enabled = true/" "$etc/config.toml"
            ;;
    esac
done < /root/.ninja-secrets
'
fi

echo "Starting ninja-ingest..."
in_ct "cd /root/ninja-ingest && ./install-ingest.sh --start" | sed 's/^/  /'

if [ "$cloudflared" = yes ]; then
    echo "Installing cloudflared..."
    in_ct '
set -eu
export DEBIAN_FRONTEND=noninteractive
mkdir -p --mode=0755 /usr/share/keyrings
curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg -o /usr/share/keyrings/cloudflare-main.gpg
codename=$(. /etc/os-release && echo "$VERSION_CODENAME")
echo "deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared $codename main" \
    > /etc/apt/sources.list.d/cloudflared.list
apt-get update -qq
apt-get install -y -qq cloudflared >/dev/null
token=$(sed -n "s/^TUNNEL_TOKEN=//p" /root/.ninja-secrets)
cloudflared service install "$token" >/dev/null
sleep 3
systemctl is-active --quiet cloudflared && echo "  cloudflared is running" || {
    echo "  cloudflared is not running:"; journalctl -u cloudflared -n 20 --no-pager -o cat | sed "s/^/    /"; }
'
fi
in_ct 'rm -f /root/.ninja-secrets' || true

ct_ip=$(pct exec "$ctid" -- hostname -I 2>/dev/null | awk '{ print $1 }')
echo
echo "Done. Container $ctid ($hostname) is up at ${ct_ip:-<no address yet>}."
echo
echo "Agent tokens (shown once here; saved in the container's /etc/ninja-ingest/env):"
printf '%s\n' "$tokens"
echo
echo "Next steps:"
if [ "$cloudflared" = yes ]; then
    echo "  1. In Zero Trust > Networks > Tunnels, add a public hostname to the tunnel"
    echo "     (for example ingest.example.net) with service http://localhost:9091."
    echo "  2. In Access > Applications, protect the read routes and leave /v1/ingest open:"
    echo "       ingest.example.net/v1/ingest   Bypass, Everyone"
    echo "       ingest.example.net             Allow, your login"
    echo "  3. On each agent: sudo ./install.sh --push --start"
    echo "     URL: https://ingest.example.net/v1/ingest   token: from the list above"
elif [ "$expose" = yes ]; then
    echo "  1. In your existing tunnel or reverse proxy, point the public hostname at"
    echo "     http://${ct_ip:-<container ip>}:9091 and protect everything except /v1/ingest."
    echo "  2. On each agent: sudo ./install.sh --push --start with that URL and the token above."
else
    echo "  1. Put TLS in front of localhost:9091 inside the container (see the README)."
    echo "  2. On each agent: sudo ./install.sh --push --start with that URL and the token above."
fi
echo
echo "Useful afterwards:"
echo "  pct enter $ctid                                   shell inside the container"
echo "  pct exec $ctid -- journalctl -u ninja-ingest -f   follow the log"
echo "  pct exec $ctid -- curl -s localhost:9091/hosts    who is reporting"
echo "  pct exec $ctid -- /root/ninja-ingest/install-ingest.sh --add-agent <name> --start"
