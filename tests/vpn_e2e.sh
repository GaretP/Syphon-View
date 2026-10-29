#!/usr/bin/env bash
# The app's own WireGuard against a real VPN box, run as root. Three network namespaces: this computer on the
# "internet", the VPN box (set up with syphon-vpn, real WireGuard, its real firewall and welcome desk), and the Syphon
# server at home. SYPHON_VPN points at the syphon-vpn source (default: ../syphon-vpn).
set -u
cd "$(dirname "$0")/.."
APP="$PWD"; VPNSRC="$(cd "${SYPHON_VPN:-../syphon-vpn}" && pwd)"
PASS=0; FAIL=0
ok() { echo "  ok   $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL $1"; FAIL=$((FAIL+1)); }
T="$(mktemp -d)"; chmod 700 "$T"
cleanup() {
  for n in client box node; do ip netns pids $n 2>/dev/null | xargs -r kill 2>/dev/null; ip netns del $n 2>/dev/null; done
  rm -rf "$T"
}
trap cleanup EXIT
export WG_QUICK_USERSPACE_IMPLEMENTATION=wireguard-go WG_I_PREFER_BUGGY_USERSPACE_TO_POLISHED_KMOD=1
(cd "$VPNSRC" && cargo build -q --offline) || exit 1
cargo build -q --release --example vpn_check --example headless || exit 1
VPN="$VPNSRC/target/debug/syphon-vpn --root $T/box"
CHECK="$APP/target/release/examples/vpn_check"

for n in client box node; do ip netns add $n; ip -n $n link set lo up; done
ip link add c0 netns client type veth peer name b0 netns box
ip link add b1 netns box type veth peer name n0 netns node
ip -n client addr add 203.0.113.2/24 dev c0; ip -n client link set c0 up
ip -n box addr add 203.0.113.1/24 dev b0; ip -n box link set b0 up
ip -n box addr add 192.168.50.1/24 dev b1; ip -n box link set b1 up
ip -n node addr add 192.168.50.20/24 dev n0; ip -n node link set n0 up
# a little distance between this computer and home, like a real internet connection
ip netns exec client tc qdisc add dev c0 root netem delay 15ms 2>/dev/null && ip netns exec box tc qdisc add dev b0 root netem delay 15ms 2>/dev/null && RTT="30 ms" || RTT="none"
mkdir -p "$T/www"; head -c 40000000 /dev/urandom >"$T/www/big"; BIGSUM="$(sha256sum "$T/www/big" | cut -d' ' -f1)"
cat >"$T/web.py" <<'PY'
import http.server, sys, os
class H(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/api/status":
            self.send_response(200); self.send_header("content-type", "application/json"); self.end_headers(); self.wfile.write(b'{"name":"Test Syphon"}'); return
        if self.path == "/who":
            self.send_response(200); self.end_headers(); self.wfile.write(self.client_address[0].encode()); return
        super().do_GET()
    def log_message(self, *a): pass
os.chdir(sys.argv[3])
http.server.ThreadingHTTPServer((sys.argv[1], int(sys.argv[2])), H).serve_forever()
PY
ip netns exec node python3 "$T/web.py" 0.0.0.0 8096 "$T/www" & WEB=$!

$VPN init --endpoint 203.0.113.1 --node 192.168.50.20 >/dev/null
CODE="$($VPN add "test laptop")"
chmod 600 "$T/box/syphon0.conf"
ip netns exec box sysctl -q -w net.ipv4.ip_forward=1
ip netns exec box nft -f "$T/box/syphon-vpn/firewall.nft"
ip netns exec box wg-quick up "$T/box/syphon0.conf" >/dev/null 2>&1 || { bad "the box's WireGuard comes up"; exit 1; }
ip netns exec box $VPN serve >"$T/desk.log" 2>&1 &
sleep 1

r="$(ip netns exec client timeout 30 "$CHECK" welcome-get "$CODE" - /who 2>&1)"
[[ "$r" != "192.168.50.1" ]] && ok "with only the code's welcome key, the app can't reach the Syphon server" || bad "welcome key alone reaches the server"
r="$(ip netns exec client timeout 60 "$CHECK" enroll "$CODE" "$T/own.key" 2>&1)"
[[ "$r" == enrolled ]] && ok "the app's first contact puts its own key in place" || { bad "first contact ($r)"; cat "$T/desk.log"; }
sleep 2
r="$(ip netns exec client timeout 30 "$CHECK" get "$CODE" "$T/own.key" /who 2>&1)"
[[ "$r" == "192.168.50.1" ]] && ok "with its own key the app reaches the Syphon server, which sees the box's address" || bad "own key reaches the server ($r)"
r="$(ip netns exec client timeout 120 "$CHECK" get "$CODE" "$T/own.key" /big 2>&1)"
set -- $r
[[ "${2:-}" == "$BIGSUM" ]] && ok "a 40 MB download arrives intact (${3:-?} Mbit/s, test network delay: $RTT)" || bad "40 MB download ($r)"
r="$(ip netns exec client timeout 30 "$CHECK" enroll "$CODE" "$T/own2.key" 2>&1)"
[[ "$r" != enrolled ]] && ok "the code can't be used a second time" || bad "the code worked twice"
grep -q "$(cd "$VPNSRC" && cat "$T/own.key" | wg pubkey)" "$T/box/syphon0.conf" && ok "the box holds the app's own public key" || bad "the box holds the app's own public key"

# the whole app: its helper, given a fresh code the way its window does, then used like the window uses it
CODE2="$($VPN add "test desktop")"
# (on a real box "add" does this itself; with --root it only writes the files)
ip netns exec box wg syncconf syphon0 <(ip netns exec box wg-quick strip "$T/box/syphon0.conf") && ip netns exec box nft -f "$T/box/syphon-vpn/firewall.nft"
mkdir -p "$T/appdata"
ip netns exec client "$APP/target/release/examples/headless" "$T/appdata" >"$T/headless.out" 2>"$T/headless.log" &
sleep 1; read -r HP HK <"$T/headless.out"
A() { ip netns exec client curl -s -m 60 -H "cookie: syphon_app=$HK" "$@"; }
r="$(A -X POST -H 'content-type: application/json' -d "{\"code\":\"$CODE2\"}" "http://127.0.0.1:$HP/__app/api/node")"
[[ "$r" == *'"ok":true'* ]] && ok "the app takes a VPN code (first contact, own key)" || { bad "the app takes a VPN code ($r)"; cat "$T/headless.log"; }
python3 -c "import json,sys;n=json.load(open(sys.argv[1]))['node'];assert n['vpn']['private'] and not n.get('vpn_pending')" "$T/appdata/syphon-app.json" && ! grep -q "$(python3 -c "import sys,json,base64;b=sys.argv[1].split(':',1)[1];print(json.loads(base64.urlsafe_b64decode(b+'='*(-len(b)%4)))['private'])" "$CODE2")" "$T/appdata/syphon-app.json" \
  && ok "the app keeps its own key, not the code's one-time key" || bad "the app keeps its own key"
r="$(A -X POST "http://127.0.0.1:$HP/__app/api/connect")"
[[ "$r" == *'"via":"vpn"'* ]] && ok "away from home, the app connects through the VPN box" || { bad "connects through the VPN box ($r)"; cat "$T/headless.log"; }
r="$(A "http://127.0.0.1:$HP/who")"
[[ "$r" == "192.168.50.1" ]] && ok "pages from the Syphon server come through the app" || bad "pages come through the app ($r)"
r="$(A -o "$T/got" -w '%{size_download} %{speed_download}' "http://127.0.0.1:$HP/big")"
[[ "$(sha256sum "$T/got" | cut -d' ' -f1)" == "$BIGSUM" ]] && ok "a 40 MB download through the app arrives intact ($(python3 -c "print(round(${r#* }*8/1e6))") Mbit/s)" || bad "40 MB through the app ($r)"
s1=$(date +%s%N); P=(); for i in 1 2 3 4; do A -o "$T/got$i" "http://127.0.0.1:$HP/big" & P+=($!); done; wait "${P[@]}"; s2=$(date +%s%N)
n=0; for i in 1 2 3 4; do [[ "$(sha256sum "$T/got$i" | cut -d' ' -f1)" == "$BIGSUM" ]] && n=$((n+1)); done
[[ $n -eq 4 ]] && ok "four downloads at once all arrive intact ($(( 160*8*1000 / ((s2-s1)/1000000) )) Mbit/s together)" || bad "four downloads at once ($n of 4 intact)"
r="$(A -X POST "http://127.0.0.1:$HP/__app/api/vpn/test")"
[[ "$r" == '{"ok":true'* ]] && ok "the app's connection test passes every step" || bad "connection test ($r)"
# the app restarts: it reconnects with its saved key, no code needed
ip netns pids client | xargs -r kill 2>/dev/null; sleep 1
ip netns exec client "$APP/target/release/examples/headless" "$T/appdata" >"$T/headless.out" 2>>"$T/headless.log" &
sleep 1; read -r HP HK <"$T/headless.out"
r="$(A "http://127.0.0.1:$HP/who")"
[[ "$r" == "192.168.50.1" ]] && ok "after a restart the app reconnects by itself" || bad "reconnects after a restart ($r)"
# sleep: the node's web server stops; it only comes back when a correct wake packet for its card arrives (3 s "boot")
NODEMAC="$(ip -n node link show n0 2>/dev/null | awk '/ether/ {print $2}')"
cat >"$T/sleeper.py" <<'PY2'
import socket, subprocess, sys, time
mac, web, www, log = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
want = b"\xff" * 6 + bytes.fromhex(mac.replace(":", "")) * 16
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("0.0.0.0", 9))
while True:
    d, a = s.recvfrom(2048)
    if d == want:
        open(log, "a").write(a[0] + "\n"); time.sleep(3)
        p = subprocess.Popen(["python3", web, "0.0.0.0", "8096", www]); p.wait(); break
PY2
asleep() { pkill -P $WEB 2>/dev/null; kill $WEB 2>/dev/null; sleep 0.5; : >"$T/woke.log"; ip netns exec node python3 "$T/sleeper.py" "$NODEMAC" "$T/web.py" "$T/www" "$T/woke.log" & WEB=$!; sleep 0.5; }
set_store() { python3 -c "
import json,sys; f=sys.argv[1]; d=json.load(open(f)); n=d['node']; n['mac']=sys.argv[2]; n['sleeps']=True
if len(sys.argv)>3: n['lan']=[sys.argv[3]]
json.dump(d,open(f,'w'))" "$T/appdata/syphon-app.json" "$@"; }
restart_app() { ip netns pids client | xargs -r kill 2>/dev/null; sleep 1; ip netns exec client "$APP/target/release/examples/headless" "$T/appdata" >"$T/headless.out" 2>>"$T/headless.log" & sleep 1; read -r HP HK <"$T/headless.out"; }
$VPN wake-mac "$NODEMAC" >/dev/null
# away: the box wakes it when the app connects
asleep; set_store "$NODEMAC"; restart_app
s1=$(date +%s); r="$(A -m 120 -X POST "http://127.0.0.1:$HP/__app/api/connect")"; s2=$(date +%s)
[[ "$r" == *'"via":"vpn"'* && "$(cat "$T/woke.log")" == "192.168.50.1" ]] && ok "away from home, opening the app wakes the sleeping node through the box ($((s2-s1)) s)" || bad "wake through the box ($r; woke: $(cat "$T/woke.log"))"
# at home: the app wakes it itself (a second network: this computer and the node side by side)
ip link add h0 netns client type veth peer name n1 netns node
ip -n client addr add 192.168.60.5/24 dev h0; ip -n client link set h0 up
ip -n node addr add 192.168.60.20/24 dev n1; ip -n node link set n1 up
HOMEMAC="$(ip -n node link show n1 2>/dev/null | awk '/ether/ {print $2}')"
NODEMAC="$HOMEMAC"; asleep; set_store "$HOMEMAC" "http://192.168.60.20:8096"; restart_app
s1=$(date +%s); r="$(A -m 120 -X POST "http://127.0.0.1:$HP/__app/api/connect")"; s2=$(date +%s)
[[ "$r" == *'"via":"home"'* && "$(cat "$T/woke.log")" == 192.168.60.5 ]] && ok "at home, the app wakes the sleeping node itself and connects directly ($((s2-s1)) s)" || bad "wake at home ($r; woke: $(cat "$T/woke.log"))"
set_store "$HOMEMAC" "http://192.168.50.20:8096"
# the box removes the device: the app is cut off and says so
$VPN remove "test desktop" >/dev/null && ip netns exec box wg syncconf syphon0 <(ip netns exec box wg-quick strip "$T/box/syphon0.conf") && ip netns exec box nft -f "$T/box/syphon-vpn/firewall.nft"
ip netns pids client | xargs -r kill 2>/dev/null; sleep 1
ip netns exec client "$APP/target/release/examples/headless" "$T/appdata" >"$T/headless.out" 2>>"$T/headless.log" &
sleep 1; read -r HP HK <"$T/headless.out"
r="$(A -X POST "http://127.0.0.1:$HP/__app/api/connect")"
[[ "$r" == *"VPN box did not answer"* ]] && ok "a removed device is cut off, with a clear message" || bad "removed device ($r)"
r="$(A -X POST "http://127.0.0.1:$HP/__app/api/vpn/test")"
python3 -c "import json,sys;r=json.loads(sys.argv[1]);bad=[x for x in r['steps'] if not x['ok']];assert not r['ok'] and bad[0]['step']=='VPN box answers' and 'Packets arriving' in bad[0]['detail']" "$r" \
  && ok "the connection test says where it stops (the box doesn't answer) and what to check" || bad "connection test after removal ($r)"
r="$(A -X POST -H 'content-type: application/json' -d '{"code":"SYPHON-CONNECT-1:abc"}' "http://127.0.0.1:$HP/__app/api/node")"
[[ "$r" == *"old Tor connection code"* ]] && ok "an old Tor code is explained, not used" || bad "old Tor code ($r)"
echo; echo "passed: $PASS  failed: $FAIL"
[[ $FAIL -eq 0 ]]
