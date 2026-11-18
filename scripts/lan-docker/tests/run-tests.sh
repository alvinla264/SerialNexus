#!/bin/sh
# Harness tests for scripts/lan-docker/run.sh.
#
# No Docker daemon is needed: a stub `docker` records every call and plays the
# orchestration's failure modes. These tests cover the parts of the gate that
# are easy to get wrong without a daemon — startup ordering, failure
# propagation, timeouts, cleanup scope on every exit path, and diagnostics that
# never print pairing material. The real two-container assertions live in
# `docs/lan-docker.md` and require a daemon.
#
# Run: sh scripts/lan-docker/tests/run-tests.sh
set -eu

root=$(cd "$(dirname "$0")/../../.." && pwd)
runner="$root/scripts/lan-docker/run.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

checks=0
failures=0
pass() { checks=$((checks + 1)); printf 'ok %s\n' "$1"; }
failed() {
    failures=$((failures + 1))
    printf 'FAIL %s\n' "$1"
    if [ -n "${2:-}" ]; then
        printf '%s\n' "$2" | sed 's/^/    /'
    fi
}

stub_dir="$work/bin"
stub_data="$work/stub-data"
mkdir -p "$stub_dir" "$stub_data"
cat >"$stub_dir/docker" <<'STUB'
#!/bin/sh
# A recording, scenario-driven stand-in for the Docker CLI. It models the
# resources, their `snx-lan-run` labels and the daemon's absence errors, so the
# ownership checks can be exercised without a daemon.
printf '%s\n' "$*" >>"$STUB_LOG"
scenario="${STUB_SCENARIO:-pass}"
res="$STUB_DATA/res"
mkdir -p "$res"

arg_after() {
    printf '%s\n' "$*" | sed -n "s/.* $1 \([^ ]*\).*/\1/p" | head -1 | sed 's/ .*//'
}
last_arg() { for a in "$@"; do :; done; printf '%s' "$a"; }
label_value() { arg_after --label "$@" | sed 's/^snx-lan-run=//'; }
resfile() { printf '%s/%s' "$res" "$1"; }
mark() { printf '%s\n' "$2" >"$(resfile "$1")"; }
unmark() { rm -f "$(resfile "$1")"; }
label_of() { cat "$(resfile "$1")" 2>/dev/null; }
# `conflict` pretends every name is already taken by another run.
# `race-conflict` lets pre-checks pass, then makes a container create collide.
label_for() {
    case "$scenario" in
        conflict) echo "other-run"; return 0 ;;
    esac
    label_of "$2"
}
missing() { echo "Error response from daemon: No such $1: $2" >&2; exit 1; }
# After the run's assertions, `dns-down` makes every daemon call fail with a
# connection/DNS error, so cleanup must not mistake it for an absent resource.
daemon_down() {
    if [ -f "$STUB_DATA/dns-down" ]; then
        echo "dial tcp: lookup docker.example: no such host" >&2
        return 0
    fi
    if [ -f "$STUB_DATA/api-404" ]; then
        echo "404 page not found" >&2
        return 0
    fi
    return 1
}

case "${1:-}" in
    version)
        if [ "$scenario" = "no-daemon" ]; then
            echo "Cannot connect to the Docker daemon at unix:///stub.sock" >&2
            exit 1
        fi
        echo "29.0.0-stub"
        ;;
    build)
        if [ "$scenario" = "build-hang" ]; then
            sleep 30
            exit 1
        fi
        if [ "$scenario" = "build-fails" ]; then
            echo "stub build failed" >&2
            exit 1
        fi
        daemon_down && exit 1
        tag=$(arg_after --tag "$@")
        mark "$tag" "$(label_value "$@")"
        echo "stub-image-id"
        ;;
    network)
        case "${2:-}" in
            create)
                daemon_down && exit 1
                name=$(last_arg "$@")
                mark "$name" "$(label_value "$@")"
                echo "stub-network-id"
                ;;
            inspect)
                daemon_down && exit 1
                format="$4"
                name="$5"
                case "$format" in
                    *Labels*) if label=$(label_for network "$name"); then echo "$label"; else missing network "$name"; fi ;;
                    *Internal*) echo "true" ;;
                    *Driver*) echo "bridge" ;;
                    *) echo "bridge" ;;
                esac
                ;;
            rm) unmark "$(last_arg "$@")" ;;
        esac
        ;;
    volume)
        case "${2:-}" in
            create)
                daemon_down && exit 1
                name=$(last_arg "$@")
                mark "$name" "$(label_value "$@")"
                echo "stub-volume"
                ;;
            inspect)
                daemon_down && exit 1
                format="$4"
                name="$5"
                case "$format" in
                    *Labels*) if label=$(label_for volume "$name"); then echo "$label"; else missing volume "$name"; fi ;;
                    *) echo "stub" ;;
                esac
                ;;
            rm) unmark "$(last_arg "$@")" ;;
        esac
        ;;
    create)
        daemon_down && exit 1
        name=$(arg_after --name "$@")
        if [ "$scenario" = "race-conflict" ]; then
            mark "$name" "other-run"
            echo "Conflict. The container name \"$name\" is already in use." >&2
            exit 1
        fi
        mark "$name" "$(label_value "$@")"
        printf '%s\n' "$*" >"$STUB_DATA/$name.create"
        echo "stub-container-$name"
        ;;
    start)
        name="${2:-}"
        printf '%s\n' "$*" >"$STUB_DATA/$name.start"
        ;;
    wait)
        name="${2:-}"
        if [ "$scenario" = "dns-down" ]; then
            # The assertions finish, then the daemon becomes unreachable.
            : >"$STUB_DATA/dns-down"
            echo 0
            exit 0
        fi
        if [ "$scenario" = "api-404" ]; then
            # A misrouted daemon endpoint answers generically, not with an
            # absence message about this resource.
            : >"$STUB_DATA/api-404"
            echo 0
            exit 0
        fi
        case "$scenario" in
            hang) sleep 30 ;;
            client-fail)
                case "$name" in *-client) echo 3; exit 0 ;; esac
                echo 0
                ;;
            *) echo 0 ;;
        esac
        ;;
    logs)
        if [ "$scenario" = "secret-logs" ]; then
            echo "lan-gate: host: sharing session 1 listening on 172.18.0.2:7331; pairing keys written privately"
            echo "lan-gate: leaked snx1.human.eyJ2IjoxfQ and credential=abcdef0123456789abcdef"
            echo "lan-gate: client: PASS 1 stub step"
        else
            if [ "$scenario" != "no-announce" ]; then
                echo "lan-gate: host: sharing session 1 listening on 172.18.0.2:7331; pairing keys written privately"
            fi
            echo "lan-gate: client: PASS 1 stub step"
        fi
        ;;
    inspect)
        daemon_down && exit 1
        format="$3"
        name="$4"
        case "$format" in
            *Labels*) if label=$(label_for container "$name"); then echo "$label"; else missing object "$name"; fi ;;
            *Privileged*) echo "false" ;;
            *PortBindings*) echo "0" ;;
            *Devices*) echo "0" ;;
            *NetworkMode*) sed -n 's/.*--network \([^ ]*\).*/\1/p' "$STUB_DATA/$name.create" ;;
            *CapDrop*) echo "[ALL]" ;;
            *) echo "stub" ;;
        esac
        ;;
    image)
        case "${2:-}" in
            inspect)
                daemon_down && exit 1
                format="$4"
                name="$5"
                case "$format" in
                    *Labels*) if label=$(label_for image "$name"); then echo "$label"; else missing image "$name"; fi ;;
                    *) echo "stub" ;;
                esac
                ;;
            rm) unmark "$(last_arg "$@")" ;;
        esac
        ;;
    rm)
        if [ "$scenario" = "rm-fails" ]; then
            echo "Error response from daemon: device or resource busy" >&2
            exit 1
        fi
        unmark "$(last_arg "$@")"
        ;;
    *) ;;
esac
STUB
chmod +x "$stub_dir/docker"

has() { grep -F -- "$1" "$2" >/dev/null 2>&1; }

run_case() {
    case_name=$1
    scenario=$2
    shift 2
    : >"$work/$case_name.log"
    case_data="$stub_data/$case_name"
    rm -rf "$case_data"
    mkdir -p "$case_data"
    set +e
    PATH="$stub_dir:$PATH" \
        STUB_LOG="$work/$case_name.log" \
        STUB_DATA="$case_data" \
        STUB_SCENARIO="$scenario" \
        DOCKER_CLI="$stub_dir/docker" \
        SNX_LAN_BUILD_TIMEOUT=10 \
        SNX_LAN_START_TIMEOUT=3 \
        SNX_LAN_CLIENT_TIMEOUT=3 \
        SNX_LAN_SHUTDOWN_TIMEOUT=3 \
        SNX_LAN_DOCKER_TIMEOUT=10 \
        sh "$runner" "$@" >"$work/$case_name.out" 2>&1
    status=$?
    set -e
}

# 1. No daemon: fail early, create nothing.
run_case no-daemon no-daemon
if [ "$status" -ne 0 ] && has "Cannot connect to the Docker daemon" "$work/no-daemon.out" &&
    ! has " create " "$work/no-daemon.log"; then
    pass "a missing daemon fails before creating anything"
else
    failed "a missing daemon fails before creating anything" "$(cat "$work/no-daemon.out")"
fi

# 2. The happy path: host starts before the client, no privileged flags, and
#    only the resources this run named are removed.
run_case pass pass
if [ "$status" -eq 0 ] && has "client: PASS 1 stub step" "$work/pass.out"; then
    pass "a passing run exits zero and shows the gate's PASS lines"
else
    failed "a passing run exits zero and shows the gate's PASS lines" "$(cat "$work/pass.out")"
fi
host_start=$(grep -n '^start snx-lan-.*-host$' "$work/pass.log" | head -1 | cut -d: -f1)
client_start=$(grep -n '^start snx-lan-.*-client$' "$work/pass.log" | head -1 | cut -d: -f1)
if [ -n "$host_start" ] && [ -n "$client_start" ] && [ "$host_start" -lt "$client_start" ]; then
    pass "the host container starts before the client"
else
    failed "the host container starts before the client" "$(cat "$work/pass.log")"
fi
if has "network create" "$work/pass.log" &&
    has "snx-lan-" "$work/pass.log" &&
    ! has "--privileged" "$work/pass.log" &&
    ! has "--network host" "$work/pass.log" &&
    ! has "--publish" "$work/pass.log" &&
    ! has "--device" "$work/pass.log" &&
    has "--cap-drop ALL" "$work/pass.log" &&
    has "--security-opt no-new-privileges" "$work/pass.log" &&
    has "--read-only" "$work/pass.log"; then
    pass "containers are unprivileged, unpublished, device-free and on the private network"
else
    failed "containers are unprivileged, unpublished, device-free and on the private network" "$(cat "$work/pass.log")"
fi
if has "volume create" "$work/pass.log" &&
    has "volume rm" "$work/pass.log" &&
    has "rm -f" "$work/pass.log" &&
    has "network rm" "$work/pass.log" &&
    has "image rm" "$work/pass.log" &&
    has "removed: containers" "$work/pass.out" &&
    has "removed: volume" "$work/pass.out"; then
    pass "a finished run removes its containers, network, volume and image"
else
    failed "a finished run removes its containers, network, volume and image" "$(cat "$work/pass.log")"
fi

# 3. A host that never announces times out, fails and still cleans up.
run_case no-announce no-announce
if [ "$status" -ne 0 ] &&
    has "announced no sharing session" "$work/no-announce.out" &&
    has "rm -f" "$work/no-announce.log" &&
    has "network rm" "$work/no-announce.log"; then
    pass "a host that never announces times out with a scoped cleanup"
else
    failed "a host that never announces times out with a scoped cleanup" "$(cat "$work/no-announce.out")"
fi

# 4. A failing client propagates its exit code and cleans up.
run_case client-fail client-fail
if [ "$status" -ne 0 ] &&
    has "client container exited 3" "$work/client-fail.out" &&
    has "rm -f" "$work/client-fail.log"; then
    pass "a failing client is propagated with a non-zero exit"
else
    failed "a failing client is propagated with a non-zero exit" "$(cat "$work/client-fail.out")"
fi

# 5. A build failure creates no containers.
run_case build-fails build-fails
if [ "$status" -ne 0 ] &&
    has "image build failed" "$work/build-fails.out" &&
    ! has " create " "$work/build-fails.log"; then
    pass "a failed image build creates no containers"
else
    failed "a failed image build creates no containers" "$(cat "$work/build-fails.out")"
fi

# 6. Interruption mid-wait cleans up both containers, the network and the
#    private state directory, and exits non-zero.
mkdir -p "$stub_data/interrupt"
: >"$work/interrupt.log"
set +e
PATH="$stub_dir:$PATH" \
    STUB_LOG="$work/interrupt.log" \
    STUB_DATA="$stub_data/interrupt" \
    STUB_SCENARIO=hang \
    DOCKER_CLI="$stub_dir/docker" \
    SNX_LAN_BUILD_TIMEOUT=10 \
    SNX_LAN_START_TIMEOUT=3 \
    SNX_LAN_CLIENT_TIMEOUT=60 \
    SNX_LAN_SHUTDOWN_TIMEOUT=3 \
    SNX_LAN_DOCKER_TIMEOUT=10 \
    sh "$runner" >"$work/interrupt.out" 2>&1 &
runner_pid=$!
i=0
while [ "$i" -lt 100 ] && ! grep -q '^wait snx-lan-.*-client$' "$work/interrupt.log"; do
    i=$((i + 1))
    sleep 0.1
done
kill -TERM "$runner_pid" 2>/dev/null
wait "$runner_pid"
status=$?
set -e
if [ "$status" -ne 0 ] && has "rm -f" "$work/interrupt.log" &&
    has "network rm" "$work/interrupt.log" &&
    has "volume rm" "$work/interrupt.log" &&
    has "interrupted" "$work/interrupt.out"; then
    pass "an interrupt cleans up its own containers, network and state volume"
else
    failed "an interrupt cleans up its own containers, network and state volume" "$(cat "$work/interrupt.out")"
fi

# 7. Diagnostics redact anything that looks like pairing material.
run_case secret-logs secret-logs
if [ "$status" -eq 0 ] &&
    has "<redacted>" "$work/secret-logs.out" &&
    ! has "snx1.human.eyJ2IjoxfQ" "$work/secret-logs.out" &&
    ! has "abcdef0123456789abcdef" "$work/secret-logs.out"; then
    pass "diagnostics redact sharing keys and credentials"
else
    failed "diagnostics redact sharing keys and credentials" "$(cat "$work/secret-logs.out")"
fi

# 8. --keep leaves the created resources alone and names them, on success and
#    on failure (the documented way to inspect a failed run).
run_case keep pass --keep
run_case keep-failed client-fail --keep
if [ "$status" -ne 0 ] &&
    [ -s "$work/keep.out" ] &&
    ! has "rm -f" "$work/keep.log" &&
    ! has "network rm" "$work/keep.log" &&
    ! has "volume rm" "$work/keep.log" &&
    has "kept" "$work/keep.out" &&
    has "kept" "$work/keep-failed.out"; then
    pass "--keep leaves the named resources for inspection, including on failure"
else
    failed "--keep leaves the named resources for inspection, including on failure" "$(cat "$work/keep.out" "$work/keep-failed.out")"
fi

# 9. A cleanup failure is reported and makes the run non-zero; it is never
#    claimed as removed.
run_case rm-fails rm-fails
if [ "$status" -ne 0 ] &&
    has "cleanup could not remove" "$work/rm-fails.out" &&
    has "device or resource busy" "$work/rm-fails.out" &&
    ! has "removed: containers" "$work/rm-fails.out"; then
    pass "a cleanup failure is reported and fails the run"
else
    failed "a cleanup failure is reported and fails the run" "$(cat "$work/rm-fails.out")"
fi

# 10. Interrupting a running build removes the temporary build log too.
mkdir -p "$work/tmp" "$stub_data/build-interrupt"
: >"$work/build-interrupt.log"
set +e
PATH="$stub_dir:$PATH" \
    STUB_LOG="$work/build-interrupt.log" \
    STUB_DATA="$stub_data/build-interrupt" \
    STUB_SCENARIO=build-hang \
    DOCKER_CLI="$stub_dir/docker" \
    TMPDIR="$work/tmp" \
    SNX_LAN_BUILD_TIMEOUT=60 \
    SNX_LAN_START_TIMEOUT=3 \
    SNX_LAN_CLIENT_TIMEOUT=3 \
    SNX_LAN_SHUTDOWN_TIMEOUT=3 \
    SNX_LAN_DOCKER_TIMEOUT=10 \
    sh "$runner" >"$work/build-interrupt.out" 2>&1 &
runner_pid=$!
i=0
while [ "$i" -lt 100 ] && ! grep -q '^build ' "$work/build-interrupt.log"; do
    i=$((i + 1))
    sleep 0.1
done
kill -TERM "$runner_pid" 2>/dev/null
wait "$runner_pid"
status=$?
set -e
if [ "$status" -ne 0 ] && has "interrupted" "$work/build-interrupt.out" &&
    [ -z "$(ls "$work/tmp"/snx-lan-build.* 2>/dev/null)" ]; then
    pass "an interrupted build removes its temporary log"
else
    failed "an interrupted build removes its temporary log" "$(cat "$work/build-interrupt.out"; ls "$work/tmp")"
fi

# 11. A name that already exists is never adopted or removed.
run_case conflict conflict
if [ "$status" -ne 0 ] &&
    has "already exists" "$work/conflict.out" &&
    has "refusing to adopt or remove" "$work/conflict.out" &&
    ! has "rm -f" "$work/conflict.log" &&
    ! has "network rm" "$work/conflict.log" &&
    ! has "volume rm" "$work/conflict.log" &&
    ! has "image rm" "$work/conflict.log"; then
    pass "an existing resource name is refused and never removed"
else
    failed "an existing resource name is refused and never removed" "$(cat "$work/conflict.out")"
fi

# 12. A raced name collision leaves the foreign resource alone and reports the
#     cleanup it could not complete.
run_case race-conflict race-conflict
if [ "$status" -ne 0 ] &&
    has "not owned by this run" "$work/race-conflict.out" &&
    has "left in place" "$work/race-conflict.out" &&
    has "cleanup could not remove" "$work/race-conflict.out" &&
    ! has "rm -f" "$work/race-conflict.log" &&
    has "network rm" "$work/race-conflict.log"; then
    pass "a raced name collision leaves the foreign resource in place"
else
    failed "a raced name collision leaves the foreign resource in place" "$(cat "$work/race-conflict.out")"
fi

# 13. A daemon connection/DNS failure during cleanup is not an absent
#     resource: nothing may be claimed removed and the run must fail.
run_case dns-down dns-down
if [ "$status" -ne 0 ] &&
    has "cannot verify ownership" "$work/dns-down.out" &&
    has "no such host" "$work/dns-down.out" &&
    has "cleanup could not remove" "$work/dns-down.out" &&
    ! has "removed: containers" "$work/dns-down.out"; then
    pass "a daemon DNS failure is not mistaken for absent resources"
else
    failed "a daemon DNS failure is not mistaken for absent resources" "$(cat "$work/dns-down.out")"
fi

# 14. A generic API error ("404 page not found") is not an absent resource.
run_case api-404 api-404
if [ "$status" -ne 0 ] &&
    has "cannot verify ownership" "$work/api-404.out" &&
    has "404 page not found" "$work/api-404.out" &&
    has "cleanup could not remove" "$work/api-404.out" &&
    ! has "removed: containers" "$work/api-404.out"; then
    pass "a generic daemon API error is not mistaken for absent resources"
else
    failed "a generic daemon API error is not mistaken for absent resources" "$(cat "$work/api-404.out")"
fi

# 15. Each pin is a digest, not a floating tag.
if grep -Eq '^RUST_IMAGE=[a-z]+@sha256:[0-9a-f]{64}$' "$root/scripts/lan-docker/pins.env" &&
    grep -Eq '^RUNTIME_IMAGE=[a-z]+@sha256:[0-9a-f]{64}$' "$root/scripts/lan-docker/pins.env"; then
    pass "both images are pinned by digest"
else
    failed "both images are pinned by digest" "$(cat "$root/scripts/lan-docker/pins.env" 2>/dev/null)"
fi

printf '\n%d checks, %d failed\n' "$checks" "$failures"
[ "$failures" -eq 0 ]
