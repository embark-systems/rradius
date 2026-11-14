#!/bin/sh
# Checks the netlogon handler against a Samba AD DC: PEAP-MSCHAPv2 logins with eapol_test
# through rradius, which asks the DC over a Netlogon secure channel. The DC and eapol_test run
# in a container whose ports 135 and 49200 are published on 127.0.0.1; rradius runs here.
# Needs Docker and `cargo build --release -p rradius-server -p rradius-bench` first.
# With --measure, also measures MS-CHAPv2 logins per second for several `connections`.
# Exits non-zero if any check fails.
set -u

cd "$(dirname "$0")"
root=$(cd ../.. && pwd)
server=$root/target/release/rradius-server
bench=$root/target/release/rradius-bench
image=rradius-netlogon-interop
dc=rradius-netlogon-dc
auth_port=18120
metrics_port=19812
measure=${1:-}
work=$(mktemp -d)
pid=

failed=0
check() {
    name=$1
    shift
    if output=$("$@" 2>&1); then
        echo "ok: $name"
    else
        echo "FAILED: $name"
        echo "$output" | tail -20 | sed 's/^/    /'
        failed=1
    fi
}

cleanup() {
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; fi
    if [ "$failed" -ne 0 ]; then
        echo "== rradius log"
        tail -40 "$work/rradius.log" 2>/dev/null
        echo "== DC log"
        docker logs --tail 40 "$dc" 2>&1
    fi
    docker rm -f "$dc" >/dev/null 2>&1
    rm -rf "$work"
}
trap cleanup EXIT

for program in "$server" "$bench"; do
    [ -x "$program" ] || { echo "no $program: run cargo build --release -p rradius-server -p rradius-bench"; exit 2; }
done

docker build -q -t "$image" . >/dev/null || { echo "FAILED: building $image"; exit 1; }
docker rm -f "$dc" >/dev/null 2>&1
# SYS_ADMIN: provisioning sets NT ACLs on the sysvol, which needs security.* extended attributes.
docker run -d --name "$dc" --hostname dc1 --cap-add SYS_ADMIN --add-host host.docker.internal:host-gateway \
    -p 127.0.0.1:135:135 -p 127.0.0.1:49200:49200 -v "$work:/work" "$image" >/dev/null || exit 1

wait_for_dc() {
    for _ in $(seq 1 120); do
        if docker exec "$dc" test -f /var/lib/samba/provisioned && nc -z 127.0.0.1 49200 2>/dev/null; then
            sleep 2
            return 0
        fi
        sleep 1
    done
    echo "FAILED: the DC did not start"
    failed=1
    exit 1
}
wait_for_dc
staff=$(docker exec "$dc" samba-tool group show wifi-staff --attributes=objectSid | awk '/^objectSid/ { print $2 }')
echo "wifi-staff is $staff"

# The server's EAP certificate, made with the openssl in the image.
docker exec "$dc" sh -c '
    set -e
    cd /work
    openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=Interop CA" -keyout ca.key -out ca.pem 2>/dev/null
    openssl req -newkey rsa:2048 -nodes -subj "/CN=radius.test" -keyout server.key -out server.csr 2>/dev/null
    printf "subjectAltName=DNS:radius.test\nextendedKeyUsage=serverAuth\n" > ext
    openssl x509 -req -days 2 -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -extfile ext -out server.pem 2>/dev/null
    chmod 644 server.key' || exit 1

printf 'Machine-Pass-1\n' > "$work/machine.secret"
printf "return 'reject'\n" > "$work/policy.lua"
write_config() {
    cat > "$work/rradius.toml" <<EOF
dictionary = "$root/share/dictionary"

[lua]
script = "policy.lua"

[cluster]
data_dir = "data"
secret_raft = "interop-raft-secret-0123"
secret_api = "interop-api-secret-01234"

[auth]
[[auth.handlers]]
type = "netlogon"
domain = "CORP"
servers = ["localhost"]
computer_name = "RRADIUS1"
machine_password_file = "machine.secret"
connections = $1
required_groups = ["$staff"]

[[auth.handlers.group_reply]]
group = "$staff"
attributes = { "Filter-Id" = "staff" }

[radius]
bind = "0.0.0.0"
auth_port = $auth_port
acct_port = $((auth_port + 1))

[radius.eap]
cert_file = "server.pem"
key_file = "server.key"
methods = ["peap"]

[[radius.clients]]
name = "interop"
address = "0.0.0.0/0"
secret = "interop-secret"
eap_methods = ["peap"]

[metrics]
bind = "127.0.0.1"
port = $metrics_port
EOF
}

start_rradius() {
    write_config "$1"
    rm -rf "$work/data"
    (cd "$work" && exec "$server" server --config rradius.toml >"$work/rradius.log" 2>&1) &
    pid=$!
    for _ in $(seq 1 60); do
        curl -sf "http://127.0.0.1:$metrics_port/metrics" >/dev/null && return 0
        sleep 0.5
    done
    echo "FAILED: rradius did not start"
    failed=1
    exit 1
}

stop_rradius() {
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    pid=
}

write_config 4
check "rradius check does not contact the DC" sh -c "cd '$work' && '$server' check --config rradius.toml"
start_rradius 4

# PEAP-MSCHAPv2 with eapol_test, from the container to rradius here.
eapol() {
    docker exec "$dc" sh -c "
        host=\$(getent hosts host.docker.internal | awk '{ print \$1; exit }')
        printf '%s\n' 'network={' 'key_mgmt=WPA-EAP' 'eap=PEAP' 'identity=\"$1\"' 'anonymous_identity=\"anonymous\"' 'password=\"$2\"' 'phase2=\"auth=MSCHAPV2\"' '}' > /tmp/peap.conf
        eapol_test -r0 -t15 -c /tmp/peap.conf -a \"\$host\" -p $auth_port -s interop-secret 2>&1"
}

accepted() {
    output=$(eapol "$1" "$2")
    echo "$output" | grep -q '^SUCCESS' || { echo "$output" | tail -30; return 1; }
    # The MSK eapol_test derived matches the MPPE keys in the Access-Accept.
    echo "$output" | grep -q 'MPPE keys OK: 1' || { echo "$output" | grep -i mppe; return 1; }
    [ -z "${3:-}" ] || echo "$output" | grep -q "$3" || { echo "$output" | sed -n "/Access-Accept/,\$p" | grep -i attribute; return 1; }
}

refused() {
    output=$(eapol "$1" "$2")
    # A refusal is an EAP-Failure; an unanswered request also ends in FAILURE, after a timeout.
    if echo "$output" | grep -q "timed out"; then echo "no answer (timed out)"; return 1; fi
    echo "$output" | grep -q '^FAILURE' || { echo "$output" | tail -30; return 1; }
}

metric() {
    curl -sf "http://127.0.0.1:$metrics_port/metrics" | awk -v name="$1" '$1 == name { print $2 }'
}

# eapol_test has no name for Filter-Id (11); length 7 is "staff".
check "right password: EAP-Success with an MSK and the group's reply" accepted alice Alice-Pass-1 'Attribute 11 (.*) length=7'
check "a domain in the name" accepted 'CORP\alice' Alice-Pass-1
check "wrong password refused" refused alice Wrong-Pass-1
check "disabled account refused" refused carol Carol-Pass-1
check "not in required_groups refused" refused bob Bob-Pass-1
check "unknown user refused" refused nobody Any-Pass-1
check "wrong password logged by name" grep -q STATUS_WRONG_PASSWORD "$work/rradius.log"
check "disabled account logged by name" grep -q STATUS_ACCOUNT_DISABLED "$work/rradius.log"
check "one secure channel for every login" test "$(metric 'rradius_netlogon_channel_setups_total{result="ok"}')" = 1
check "logins counted" test "$(metric 'rradius_netlogon_logons_total{result="accept"}')" = 2
check "no secrets in the log" sh -c "! grep -q -e Machine-Pass-1 -e Alice-Pass-1 '$work/rradius.log'"

# The DC restarts: the next login sets the channel up again.
docker restart "$dc" >/dev/null
wait_for_dc
check "login after the DC restarts" accepted alice Alice-Pass-1
check "the channel set up again" test "$(metric 'rradius_netlogon_channel_setups_total{result="ok"}')" -ge 2

# A wrong computer account password: the channel cannot be set up, and nothing is answered.
stop_rradius
printf 'Other-Pass-1\n' > "$work/machine.secret"
start_rradius 4
unanswered() {
    output=$(eapol "$1" "$2")
    if echo "$output" | grep -q '^SUCCESS'; then echo "$output" | tail -10; return 1; fi
}
check "wrong machine password: not accepted" unanswered alice Alice-Pass-1
check "wrong machine password logged" grep -q 'cannot set up the secure channel as RRADIUS1\$' "$work/rradius.log"
stop_rradius
printf 'Machine-Pass-1\n' > "$work/machine.secret"

if [ "$measure" = "--measure" ]; then
    for connections in 1 4 16; do
        start_rradius "$connections"
        rate=$("$bench" run --server 127.0.0.1 --auth-port "$auth_port" --secret interop-secret --load mschapv2,concurrency=64 \
            --user alice --password Alice-Pass-1 --warmup 2 --duration 10 2>&1 | awk '/mschapv2/ && /\/s/ { print; exit }')
        echo "connections = $connections: $rate"
        stop_rradius
    done
fi

[ "$failed" -eq 0 ] && echo "all checks passed"
exit "$failed"
