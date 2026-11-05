#!/usr/bin/env bash
# Smoke test of a ninja-monitor deploy: the agent's reports reach ninja-ingest.
# The deploy already checked on the host that /healthz reports the new version.
# This checks the rest of the path: the new binary samples, pushes, and
# ingest accepts it. It waits until ingest's GET /hosts shows <agent> sending
# "ninja-monitor/<version>" with last_seen after the test started.
# deploy.yml runs it after each staging deploy, and its result
# (the commit status staging/smoke) decides whether that version may go to prod.
#
#   deploy/staging-smoke.sh <ingest-url> <agent> <version> [timeout-seconds]
#
# CF_ACCESS_CLIENT_ID and CF_ACCESS_CLIENT_SECRET (the ci-ingest Access service
# token) are required. The token is read from a file so it never appears in a
# command line.
set -euo pipefail

base=${1:-}
agent=${2:-}
version=${3:-}
timeout=${4:-180}
if [ -z "$base" ] || [ -z "$agent" ] || [ -z "$version" ]; then
	echo "usage: deploy/staging-smoke.sh <ingest-url> <agent> <version> [timeout-seconds]" >&2
	exit 2
fi
base=${base%/}
want="ninja-monitor/${version#v}"

fail() { echo "smoke: $*" >&2; exit 1; }
if [ -z "${CF_ACCESS_CLIENT_ID:-}" ] || [ -z "${CF_ACCESS_CLIENT_SECRET:-}" ]; then
	fail "CF_ACCESS_CLIENT_ID and CF_ACCESS_CLIENT_SECRET must be set"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
(
	umask 077
	printf 'CF-Access-Client-Id: %s\nCF-Access-Client-Secret: %s\n' \
		"$CF_ACCESS_CLIENT_ID" "$CF_ACCESS_CLIENT_SECRET" > "$tmp/access"
)

# last_seen is ingest's clock in ms; allow a little skew from this runner's.
since=$(( ($(date +%s) - 5) * 1000 ))
deadline=$(( $(date +%s) + timeout ))
last=''
while :; do
	status=$(curl -sS --max-time 15 -H "@$tmp/access" -o "$tmp/body" -w '%{http_code}' \
		"$base/hosts" 2>"$tmp/error") || status=000
	case $status in
	200)
		host=$(jq -c --arg a "$agent" '.hosts[] | select(.name == $a)' "$tmp/body")
		[ -n "$host" ] || fail "ingest has no agent named $agent (is it in ingest's config?)"
		seen_agent=$(jq -r '.agent // ""' <<<"$host")
		seen_at=$(jq -r '.last_seen // 0' <<<"$host")
		if [ "$seen_agent" = "$want" ] && [ "$seen_at" -ge "$since" ]; then
			echo "smoke: ok $agent reports to ingest as $want ($host)"
			exit 0
		fi
		last="$agent: $host"
		;;
	3*) fail "$base/hosts answered $status, a redirect: Cloudflare Access refused the request; check the service token" ;;
	*) last="$base/hosts answered $status $(cat "$tmp/error")" ;;
	esac
	[ "$(date +%s)" -lt "$deadline" ] || fail "no report as $want after ${timeout}s; last: $last"
	sleep 10
done
