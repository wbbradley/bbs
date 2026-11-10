#!/usr/bin/env bash
# A local BBS federated with a local Hub, for trying send, read, reply and
# the directory by hand. Starts Hub's interop relay (namespace hub), a BBS
# server (namespace bbs) with humans will and agent scout, and the Hub
# adapter between them, writes DIR/env.sh and DIR/GUIDE.md, and runs until
# Ctrl-C, which stops everything and leaves DIR for inspection.
#
# Usage: scripts/hub-try.sh [--hub-checkout PATH] [--dir DIR] [--check]
#   --hub-checkout  a Hub checkout with scripts/interop-relay.sh
#                   (default: $BBS_HUB_CHECKOUT)
#   --dir           state directory (default: a new temporary directory);
#                   an existing one is reused, keeping its keys and mail
#   --check         run the guide's steps, report each, and exit 0 or 1
#                   (needs a new DIR)
# Everything listens on loopback, and every bbs command runs with HOME set
# inside DIR, so it never reads or writes your own BBS config.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
hub_checkout="${BBS_HUB_CHECKOUT:-}" dir="" check=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --hub-checkout) hub_checkout="$2"; shift 2 ;;
    --dir) dir="$2"; shift 2 ;;
    --check) check=1; shift ;;
    *) sed -n '2,16p' "$0" >&2; exit 2 ;;
  esac
done
[[ -n "$hub_checkout" ]] || { echo "hub-try: give --hub-checkout PATH or set BBS_HUB_CHECKOUT" >&2; exit 2; }
hub_checkout="$(cd "$hub_checkout" && pwd)"
[[ -x "$hub_checkout/scripts/interop-relay.sh" || -f "$hub_checkout/scripts/interop-relay.sh" ]] ||
  { echo "hub-try: $hub_checkout has no scripts/interop-relay.sh" >&2; exit 2; }
dir="${dir:-$(mktemp -d "${TMPDIR:-/tmp}/bbs-hub-try.XXXXXX")}"
mkdir -p "$dir"
dir="$(cd "$dir" && pwd)"
if [[ $check == 1 && -e "$dir/bbs" ]]; then
  echo "hub-try: --check needs a new DIR; $dir is already set up" >&2
  exit 2
fi
echo "hub-try: state directory $dir"
echo $$ >"$dir/hub-try.pid"

# This checkout's binaries, by absolute path. Every bbs command runs with
# HOME inside DIR, so ~/.config/bbs is DIR/home/.config/bbs.
(cd "$root" && cargo build --locked -q -p bbs-cli -p bbs-server -p bbs-hub-adapter)
bin="$root/target/debug"
# Go keeps its caches under the real HOME, so building Hub does not refetch.
GOCACHE="$(go env GOCACHE)" GOMODCACHE="$(go env GOMODCACHE)" GOPATH="$(go env GOPATH)"
export GOCACHE GOMODCACHE GOPATH
bbs="$bin/bbs" adapter_bin="$bin/bbs-hub-adapter"
home="$dir/home"
mkdir -p "$home" "$dir/bbs" "$dir/adapter"
export HOME="$home"
unset BBS_SERVER BBS_LOG HUB_AS HUB_TOKEN_DIR

relay_pid="" server_pid="" adapter_pid=""
# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
  trap - EXIT INT TERM
  [[ -n "$adapter_pid" ]] && kill "$adapter_pid" 2>/dev/null || true
  [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null || true
  # The relay script and its hub server share their own process group.
  [[ -n "$relay_pid" ]] && kill -TERM -- "-$relay_pid" 2>/dev/null || true
  wait 2>/dev/null || true
  echo "hub-try: stopped; state left in $dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# wait_for FILE PATTERN PID WHAT: until FILE has a line matching PATTERN.
wait_for() {
  local file="$1" pattern="$2" pid="$3" what="$4"
  for _ in $(seq 3000); do
    grep -q "$pattern" "$file" 2>/dev/null && return 0
    kill -0 "$pid" 2>/dev/null || { echo "hub-try: $what exited:" >&2; cat "$file" >&2; exit 1; }
    sleep 0.1
  done
  echo "hub-try: timed out waiting for $what" >&2
  exit 1
}

# 1. Hub's relay, the home of namespace hub, trusting namespace bbs. It
# builds Hub into DIR/relay and, on first run, makes bbs's installation key
# and credential there.
relay_dir="$dir/relay"
setsid bash "$hub_checkout/scripts/interop-relay.sh" --dir "$relay_dir" \
  --listen 127.0.0.1:0 --peer-namespace bbs >"$dir/relay.out" 2>&1 &
relay_pid=$!
wait_for "$dir/relay.out" '^Ctrl-C stops' "$relay_pid" "the Hub relay"
hub_addr="$(sed -n 's/^ *address: *\([^ ]*\).*/\1/p' "$dir/relay.out")"
echo "hub-try: Hub relay on $hub_addr"

# 2. BBS in namespace bbs, with its integration listener.
"$bin/bbs-server" --listen 127.0.0.1:0 --integration-listen 127.0.0.1:0 \
  --state-dir "$dir/bbs" --namespace bbs >"$dir/bbs/serve.out" 2>&1 &
server_pid=$!
wait_for "$dir/bbs/serve.out" 'integration stream on' "$server_pid" "the BBS server"
bbs_addr="$(sed -n 's/^bbs serve listening on //p' "$dir/bbs/serve.out")"
integration_addr="$(sed -n 's/^bbs serve integration stream on //p' "$dir/bbs/serve.out")"
export BBS_SERVER="$bbs_addr"
echo "hub-try: BBS on $bbs_addr (integration stream on $integration_addr)"

# Human will (this config's human), agent will/scout, integration hub.
"$bbs" init --server "$bbs_addr" --human will >/dev/null
if [[ ! -f "$dir/scout.token" ]]; then
  (umask 077; "$bbs" register --name scout --note "a test agent" |
    sed -n 's/.*bbs --token \([^ ]*\) .*/\1/p' >"$dir/scout.token")
  [[ -s "$dir/scout.token" ]] || { echo "hub-try: registering scout printed no token" >&2; exit 1; }
fi
if [[ ! -f "$dir/adapter/integration.token" ]]; then
  (umask 077; "$bbs" admin --state-dir "$dir/bbs" integration add hub >"$dir/adapter/integration.token")
  "$bbs" admin --state-dir "$dir/bbs" integration publish hub on >/dev/null
fi

# 3. The adapter, with the installation key and credential the relay made.
cat >"$dir/adapter/adapter.toml" <<EOF
namespace = "bbs"
store = "adapter.db"

[bbs]
endpoint = "http://$integration_addr"
integration_id = "hub"
credential_file = "integration.token"

[hub]
relay = "http://$hub_addr"
installation_id = "bbs-1"
key_file = "$relay_dir/bbs-install.key"
credential_file = "$relay_dir/bbs-install.cred"
EOF
"$adapter_bin" run --config "$dir/adapter/adapter.toml" >"$dir/adapter/adapter.log" 2>&1 &
adapter_pid=$!
for i in $(seq 300); do
  st="$("$adapter_bin" status --config "$dir/adapter/adapter.toml" 2>/dev/null || true)"
  if grep -q 'route registered' <<<"$st" && grep -q '^BBS catalog revision' <<<"$st"; then
    break
  fi
  kill -0 "$adapter_pid" 2>/dev/null || { echo "hub-try: the adapter exited:" >&2; cat "$dir/adapter/adapter.log" >&2; exit 1; }
  [[ $i == 300 ]] && { echo "hub-try: the adapter did not connect:" >&2; cat "$dir/adapter/adapter.log" >&2; exit 1; }
  sleep 0.1
done
echo "hub-try: adapter connected"

# 4. A shell environment acting as will on both sides.
cat >"$dir/env.sh" <<EOF
# Source in a new shell: bbs acts as human will on the test BBS, hub as
# will@hub on the test relay. HOME is inside $dir, so your own BBS and Hub
# configs are never used.
export HOME="$home"
unset BBS_LOG HUB_AS HUB_TOKEN_DIR
export BBS_SERVER="$bbs_addr"
export HUB_ADDR="$hub_addr"
export HUB_TOKEN="\$(cat "$relay_dir/will.token")"
export SCOUT_TOKEN="\$(cat "$dir/scout.token")"
export PATH="$bin:$relay_dir:\$PATH"
export HUB_TRY_DIR="$dir"
# Hub's administrator, for registering groups and adding members.
hub-admin() { HUB_TOKEN="\$(cat "$relay_dir/admin.token")" hub "\$@"; }
# BBS as the agent will/scout.
scout() { bbs --token "\$SCOUT_TOKEN" "\$@"; }
PS1="(bbs-hub-try) \${PS1-\$ }"
EOF

cat >"$dir/GUIDE.md" <<EOF
# Trying BBS and Hub together

BBS (namespace \`bbs\`) runs on $bbs_addr with human \`will\` and agent
\`will/scout\`. Hub (namespace \`hub\`) runs on $hub_addr with human
\`will@hub\` and agent \`echo@hub\`. The adapter joins them. In a new shell:

    source $dir/env.sh

Then \`bbs\` acts as will on BBS, \`scout\` (a shell function) as the agent
will/scout, \`hub\` as will@hub, and \`hub-admin\` as Hub's administrator.
Delivery takes a moment: if a read shows nothing yet, read again.

1. Hub's directory, from BBS:

       bbs directory hub
         id:echo@hub  agent  echo  [echo]
         id:will@hub  human  Will  [will]
       bbs directory hub --alias will
         id:will@hub  human  Will  [will]

2. BBS to Hub:

       bbs send --to id:will@hub "hello from bbs"
       hub read
         [TIME] from will@bbs to will (id bbs:MSGID):
         hello from bbs

3. Hub to BBS, and a reply (N is the id \`bbs read\` shows):

       hub send id:will@bbs "hello from hub"
       bbs read
         [TIME] from id:will@hub (remote) to will (id N):
         hello from hub
     N is the number in "(id N)". bbs reply takes only these BBS numbers, not
     the ids hub read shows:
       bbs reply N "thanks"
       hub read
         [TIME] from will@bbs to will (id bbs:MSGID) (reply to HUB_MSGID):
         thanks

4. A Hub group with a BBS member. Hub's administrator makes the group,
   will@hub joins it, and the administrator adds will@bbs:

       hub-admin entity register release --kind group
       hub group join release
       hub-admin group add release id:will@bbs
       hub send release "to the group"
       bbs read
         [TIME] from group id:release@hub (notice) to will (id A):
         will@bbs was added to release.
         [TIME] from id:will@hub via id:release@hub to will (id B):
         to the group
     A and B are numbers, such as 4 and 5. The notice (A) has no sender, so
     replying to it is refused:
       bbs reply A "hi notice"
         bbs: a group notice has no sender to reply to: ...
     A reply to the copy (B) goes to will@hub directly, not to the group, and
     answers his message to the group:
       bbs reply B "thanks, group"
       hub read
         (will@hub's own notices that he joined and will@bbs was added, then)
         [TIME] from will@bbs to will (id bbs:MSGID) (reply to HUB_MSGID):
         thanks, group

5. An agent's view. The agent's MCP tools and hooks read the same inbox:

       hub send id:will/scout@bbs "hello scout"
       scout read
         [TIME] from id:will@hub (remote) to scout (id N):
         hello scout
       scout reply N "scout here"
       hub read
         [TIME] from will/scout@bbs to will (id bbs:MSGID) (reply to HUB_MSGID):
         scout here

Behind the scenes:

    bbs-hub-adapter status --config $dir/adapter/adapter.toml
    hub status hub:MSGID            # Hub's delivery state of a message
    bbs agents; bbs read --archived --json

Logs: $dir/relay.out, $dir/relay/serve.log, $dir/bbs/serve.out,
$dir/home/bbs.log, $dir/adapter/adapter.log.

Stop: Ctrl-C the hub-try script (or kill \$(cat $dir/hub-try.pid)). It stops
the relay, BBS and the adapter and leaves $dir for inspection. Run it again
with --dir $dir to restart with the same keys and mail (addresses change, so
source env.sh again). Clean up with: rm -rf $dir
EOF

if [[ $check == 0 ]]; then
  echo
  cat "$dir/GUIDE.md"
  echo
  echo "hub-try: running; Ctrl-C stops everything."
  wait "$adapter_pid" "$server_pid" "$relay_pid" || true
  echo "hub-try: a process exited; see the logs in $dir." >&2
  exit 1
fi

# --check: the guide's steps, with their expected results.
# shellcheck source=/dev/null
source "$dir/env.sh"
failures=0
pass() { echo "ok:   $1"; }
fail() { echo "FAIL: $1" >&2; printf '%s\n' "$2" | sed 's/^/      /' >&2; failures=$((failures + 1)); }
# expect WHAT PATTERN CMD...: run CMD until its output, accumulated since
# reading consumes mail, has a line matching the extended regex PATTERN.
# Leaves the output in $out.
expect() {
  local what="$1" pattern="$2"; shift 2
  out=""
  for _ in $(seq 100); do
    out+="$("$@" 2>&1 || true)"$'\n'
    if grep -Eq -- "$pattern" <<<"$out"; then pass "$what"; return 0; fi
    sleep 0.2
  done
  fail "$what (wanted /$pattern/)" "$out"
  return 1
}
# has WHAT PATTERN: $out has a line matching PATTERN.
has() { if grep -Eq -- "$2" <<<"$out"; then pass "$1"; else fail "$1 (wanted /$2/)" "$out"; fi; }
# id_of PATTERN: the BBS message id on the first header in $out matching PATTERN.
id_of() { grep -E -- "$1" <<<"$out" | sed -n 's/.*(id \([0-9]*\)):$/\1/p' | head -1 || true; }
# run CMD...: run a step's command, failing the step if it fails.
run() { local o; o="$("$@" 2>&1)" || fail "$*" "$o"; }

out="$(bbs directory hub 2>&1)"
has "bbs directory hub: will@hub, a human" '^id:will@hub +human'
has "bbs directory hub: echo@hub, an agent" '^id:echo@hub +agent'
out="$(bbs directory hub --alias will 2>&1)"
if [[ "$(grep -c . <<<"$out")" == 1 ]]; then
  has "bbs directory hub --alias will: only will@hub" '^id:will@hub +human'
else
  fail "bbs directory hub --alias will: only will@hub" "$out"
fi

run bbs send --to id:will@hub "hello from bbs"
expect "hub read: bbs's message" '^hello from bbs$' hub read &&
  has "hub read: from will@bbs" '\] from will@bbs to will '

run hub send id:will@bbs "hello from hub"
if expect "bbs read: hub's message" '^hello from hub$' bbs read; then
  has "bbs read: from id:will@hub" '\] from id:will@hub \(remote\) to will \(id [0-9]+\):$'
  n="$(id_of 'from id:will@hub \(remote\) to will')"
  run bbs reply "$n" "thanks"
  expect "hub read: the reply" '^thanks$' hub read &&
    has "hub read: the reply is from will@bbs, answering hub's message" \
      '\] from will@bbs to will \(id bbs:[^)]*\) \(reply to [^)]+\):$'
fi

run hub-admin entity register release --kind group
run hub group join release
run hub-admin group add release id:will@bbs
run hub send release "to the group"
if expect "bbs read: the group copy" '^to the group$' bbs read; then
  has "bbs read: the copy is from will@hub via release@hub" \
    '\] from id:will@hub via id:release@hub to will \(id [0-9]+\):$'
  has "bbs read: the membership notice" '\] from group id:release@hub \(notice\) to will \(id [0-9]+\):$'
  b="$(id_of 'from id:will@hub via id:release@hub')"
  a="$(id_of 'from group id:release@hub \(notice\)')"
  run bbs reply "$b" "thanks, group"
  expect "hub read: the reply to the group copy" '^thanks, group$' hub read &&
    has "hub read: it reaches will@hub directly, answering his group message" \
      '\] from will@bbs to will \(id bbs:[^)]*\) \(reply to [^)]+\):$'
  if out="$(bbs reply "${a:-0}" "hi notice" 2>&1)"; then
    fail "bbs reply to the notice is refused" "$out"
  else
    has "bbs reply to the notice is refused" 'a group notice has no sender to reply to'
  fi
fi

run hub send id:will/scout@bbs "hello scout"
if expect "scout read: hub's message" '^hello scout$' scout read; then
  has "scout read: from id:will@hub" '\] from id:will@hub \(remote\) to scout \(id [0-9]+\):$'
  n="$(id_of 'from id:will@hub \(remote\) to scout')"
  run scout reply "$n" "scout here"
  expect "hub read: scout's reply" '^scout here$' hub read &&
    has "hub read: from will/scout@bbs, a reply" \
      '\] from will/scout@bbs to will \(id bbs:[^)]*\) \(reply to [^)]+\):$'
fi

if [[ $failures == 0 ]]; then
  echo "hub-try --check: all steps passed"
  exit 0
fi
echo "hub-try --check: $failures failed" >&2
exit 1
