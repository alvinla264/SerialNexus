# shellcheck shell=sh
# The Docker LAN gate's orchestration functions.
#
# Sourced by run.sh, and by the harness tests through run.sh. Everything that
# creates a Docker resource records its name before asking the daemon for it,
# so cleanup can only ever touch what this run named — including after
# SIGINT/SIGTERM, or when the CLI times out after the daemon acted.

DOCKER_CLI=${DOCKER_CLI:-docker}
IMAGE_REPO=${SNX_LAN_IMAGE_REPO:-serial-nexus-lan-gate}

BUILD_TIMEOUT=${SNX_LAN_BUILD_TIMEOUT:-1800}
START_TIMEOUT=${SNX_LAN_START_TIMEOUT:-120}
CLIENT_TIMEOUT=${SNX_LAN_CLIENT_TIMEOUT:-420}
SHUTDOWN_TIMEOUT=${SNX_LAN_SHUTDOWN_TIMEOUT:-30}
DOCKER_TIMEOUT=${SNX_LAN_DOCKER_TIMEOUT:-60}

KEEP=${KEEP:-0}
IMAGE_OVERRIDE=${IMAGE_OVERRIDE:-""}

RUN_ID=""
NETWORK=""
VOLUME=""
IMAGE=""
HOST_CONTAINER=""
CLIENT_CONTAINER=""
OWN_IMAGE=0
OWN_NETWORK=0
OWN_VOLUME=0
CREATED_CONTAINERS=""
BUILD_LOG=""

# What the host container logs once the first sharing session exists. Only this
# non-secret marker is ever read from the containers; the state volume's
# contents are never inspected by the orchestrator.
READY_MARKER="pairing keys written privately"

say() { printf 'lan-docker: %s\n' "$*"; }

# Mask anything that looks like pairing material. The gate never logs a key on
# purpose; this is the harness's own second line of defence for the case where
# a future diagnostic changes.
redact() {
    sed -E \
        -e 's/snx[0-9]+\.[a-z]+\.[A-Za-z0-9_-]+/<redacted>/g' \
        -e 's/"(human_key|ai_key|credential)"[[:space:]]*:[[:space:]]*"[^"]*"/"\1":"<redacted>"/g' \
        -e 's/(credential|credential=)[A-Za-z0-9_+\/=-]{8,}/\1<redacted>/Ig'
}

die() {
    say "FAIL: $*"
    exit 1
}

docker_now() {
    timeout "$DOCKER_TIMEOUT" "$DOCKER_CLI" "$@"
}

docker_for() {
    # docker_for SECONDS args...: one call with its own timeout.
    seconds=$1
    shift
    timeout "$seconds" "$DOCKER_CLI" "$@"
}

preflight() {
    command -v "$DOCKER_CLI" >/dev/null 2>&1 ||
        die "no Docker CLI ($DOCKER_CLI). This gate is opt-in: install Docker or set DOCKER_CLI."
    # `version` contacts the daemon. A stopped daemon, a stale socket or a
    # missing permission all surface here, before anything is created.
    version=$(docker_now version --format '{{.Server.Version}}' 2>&1) ||
        die "cannot reach the Docker daemon: $version"
    say "daemon: Docker $version"
}

new_names() {
    # A random component, not just the clock and PID: two machines or PID
    # namespaces can share a daemon, and a colliding name must never make this
    # run adopt or remove another run's resource.
    suffix=$(od -An -N4 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')
    [ -n "$suffix" ] || suffix=$(date +%s%N)
    RUN_ID="$(date +%s)-$$-$suffix"
    NETWORK="snx-lan-$RUN_ID-net"
    VOLUME="snx-lan-$RUN_ID-state"
    HOST_CONTAINER="snx-lan-$RUN_ID-host"
    CLIENT_CONTAINER="snx-lan-$RUN_ID-client"
    if [ -z "$IMAGE_OVERRIDE" ]; then
        IMAGE="$IMAGE_REPO:$RUN_ID"
    else
        IMAGE="$IMAGE_OVERRIDE"
    fi
}

build_image() {
    [ -n "$IMAGE_OVERRIDE" ] && return 0
    refuse_existing image "$IMAGE"
    OWN_IMAGE=1
    say "building $IMAGE (timeout ${BUILD_TIMEOUT}s)"
    BUILD_LOG=$(mktemp "${TMPDIR:-/tmp}/snx-lan-build.XXXXXX") || die "cannot create a build log"
    # shellcheck disable=SC2154 # repo_root is set by run.sh before sourcing
    if ! timeout "$BUILD_TIMEOUT" "$DOCKER_CLI" build \
        --file "$repo_root/crates/serial-nexus-lan-gate/Dockerfile" \
        --tag "$IMAGE" \
        --label "snx-lan-run=$RUN_ID" \
        --build-arg "RUST_IMAGE=$RUST_IMAGE" \
        --build-arg "RUNTIME_IMAGE=$RUNTIME_IMAGE" \
        "$@" "$repo_root" >"$BUILD_LOG" 2>&1; then
        tail -n 40 "$BUILD_LOG" | redact | sed 's/^/    /'
        rm -f "$BUILD_LOG"
        BUILD_LOG=""
        die "image build failed"
    fi
    rm -f "$BUILD_LOG"
    BUILD_LOG=""
}

create_network() {
    refuse_existing network "$NETWORK"
    say "network: $NETWORK (bridge, internal)"
    OWN_NETWORK=1
    if ! docker_now network create \
        --driver bridge \
        --internal \
        --label "snx-lan-run=$RUN_ID" \
        "$NETWORK" >/dev/null; then
        die "cannot create the private bridge network"
    fi
    internal=$(docker_now network inspect --format '{{.Internal}}' "$NETWORK" 2>/dev/null)
    driver=$(docker_now network inspect --format '{{.Driver}}' "$NETWORK" 2>/dev/null)
    [ "$internal" = "true" ] || die "the test network is not internal: $internal"
    [ "$driver" = "bridge" ] || die "the test network is not a bridge: $driver"
}

# The containers' only shared, writable state. A named volume avoids the
# permission mismatch between a non-root caller and a rootful daemon's
# container user, and it is private to the daemon. The orchestrator never reads
# its contents: the ready marker comes from the container log.
create_volume() {
    refuse_existing volume "$VOLUME"
    say "state: volume $VOLUME"
    OWN_VOLUME=1
    docker_now volume create --label "snx-lan-run=$RUN_ID" "$VOLUME" >/dev/null ||
        die "cannot create the private state volume"
}

create_container() {
    role=$1
    name=$2
    refuse_existing container "$name"
    CREATED_CONTAINERS="$CREATED_CONTAINERS $name"
    docker_now create \
        --name "$name" \
        --label "snx-lan-run=$RUN_ID" \
        --network "$NETWORK" \
        --cap-drop ALL \
        --security-opt no-new-privileges \
        --read-only \
        --tmpfs /tmp:mode=1777 \
        --mount "type=volume,source=$VOLUME,target=/state" \
        --stop-timeout 5 \
        "$IMAGE" "$role" >/dev/null ||
        die "cannot create the $role container"
    assert_isolation "$role" "$name"
}

# Prove the container really has the properties this gate claims.
assert_isolation() {
    role=$1
    name=$2
    privileged=$(docker_now inspect --format '{{.HostConfig.Privileged}}' "$name") ||
        die "cannot inspect the $role container"
    [ "$privileged" = "false" ] || die "the $role container is privileged"
    ports=$(docker_now inspect --format '{{len .HostConfig.PortBindings}}' "$name")
    [ "$ports" = "0" ] || die "the $role container publishes $ports port binding(s)"
    devices=$(docker_now inspect --format '{{len .HostConfig.Devices}}' "$name")
    [ "$devices" = "0" ] || die "the $role container has $devices host device(s)"
    mode=$(docker_now inspect --format '{{.HostConfig.NetworkMode}}' "$name")
    [ "$mode" = "$NETWORK" ] || die "the $role container is not on the private network: $mode"
    caps=$(docker_now inspect --format '{{.HostConfig.CapDrop}}' "$name")
    case "$caps" in
        *ALL*) ;;
        *) die "the $role container did not drop all capabilities" ;;
    esac
}

start_host() {
    docker_now start "$HOST_CONTAINER" >/dev/null || die "cannot start the host container"
    say "waiting up to ${START_TIMEOUT}s for the host's sharing session"
    deadline=$(( $(date +%s) + START_TIMEOUT ))
    while :; do
        logs=$(docker_now logs "$HOST_CONTAINER" 2>&1) || logs=""
        case "$logs" in
            *"$READY_MARKER"*) return 0 ;;
        esac
        if [ "$(date +%s)" -ge "$deadline" ]; then
            collect_logs
            die "the host announced no sharing session within ${START_TIMEOUT}s"
        fi
        sleep 0.2
    done
}

start_client() {
    docker_now start "$CLIENT_CONTAINER" >/dev/null || die "cannot start the client container"
}

wait_client() {
    say "waiting up to ${CLIENT_TIMEOUT}s for the client's assertions"
    client_status=$(docker_for "$CLIENT_TIMEOUT" wait "$CLIENT_CONTAINER") ||
        client_status="timeout"
    collect_logs
    [ "$client_status" = "0" ] || die "client container exited $client_status"
}

wait_host_shutdown() {
    host_status=$(docker_for "$SHUTDOWN_TIMEOUT" wait "$HOST_CONTAINER") ||
        host_status="timeout"
    [ "$host_status" = "0" ] || die "host container exited $host_status"
}

collect_logs() {
    for container in "$HOST_CONTAINER" "$CLIENT_CONTAINER"; do
        [ -n "$container" ] || continue
        logs=$(docker_now logs --tail 200 "$container" 2>&1) || logs=""
        [ -n "$logs" ] || continue
        say "logs: $container"
        printf '%s\n' "$logs" | redact | sed 's/^/    /'
    done
}

# The `snx-lan-run` label a resource carries, or a daemon error on stdout.
resource_label() {
    kind=$1
    name=$2
    case "$kind" in
        container) docker_now inspect --format '{{index .Config.Labels "snx-lan-run"}}' "$name" ;;
        network) docker_now network inspect --format '{{index .Labels "snx-lan-run"}}' "$name" ;;
        volume) docker_now volume inspect --format '{{index .Labels "snx-lan-run"}}' "$name" ;;
        image) docker_now image inspect --format '{{index .Config.Labels "snx-lan-run"}}' "$name" ;;
    esac
}

is_absent_error() {
    # Only an absence message for this exact resource kind counts. A generic
    # "not found" (an HTTP "404 page not found", a DNS or filesystem error) is
    # a broken daemon, not proof that the resource is gone.
    kind=$1
    lowered=$(printf '%s' "$2" | tr '[:upper:]' '[:lower:]')
    case "$kind:$lowered" in
        container:*"no such container"*|container:*"no such object"*) return 0 ;;
        network:*"no such network"*|network:*"network "*" not found"*) return 0 ;;
        volume:*"no such volume"*) return 0 ;;
        image:*"no such image"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Never adopt a name that already exists: a colliding run must not inherit or
# delete somebody else's resource.
refuse_existing() {
    kind=$1
    name=$2
    if out=$(resource_label "$kind" "$name" 2>&1); then
        die "a $kind named $name already exists (label '$out'); refusing to adopt or remove it"
    fi
    is_absent_error "$kind" "$out" ||
        die "cannot check whether $kind $name exists: $out"
}

# Remove one resource, but only after proving this run's label owns it. A
# resource the daemon never created is already in the desired state; any other
# failure is reported and makes the run non-zero.
remove_resource() {
    kind=$1
    name=$2
    shift 2
    if label=$(resource_label "$kind" "$name" 2>&1); then
        if [ "$label" != "$RUN_ID" ]; then
            CLEANUP_FAILURES="$CLEANUP_FAILURES
    $kind $name: not owned by this run (label '$label'); left in place"
            return 1
        fi
    else
        is_absent_error "$kind" "$label" && return 0
        CLEANUP_FAILURES="$CLEANUP_FAILURES
    $kind $name: cannot verify ownership: $label"
        return 1
    fi
    if out=$(docker_for "$@" 2>&1); then
        return 0
    fi
    is_absent_error "$kind" "$out" && return 0
    CLEANUP_FAILURES="$CLEANUP_FAILURES
    $kind $name: $out"
    return 1
}

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if [ "$KEEP" = "1" ]; then
        say "kept (--keep): containers$CREATED_CONTAINERS"
        say "kept (--keep): network $NETWORK volume $VOLUME image $IMAGE"
        exit "$status"
    fi
    CLEANUP_FAILURES=""
    if [ -n "$BUILD_LOG" ]; then
        rm -f "$BUILD_LOG"
        BUILD_LOG=""
    fi
    removed_containers=""
    for container in $CREATED_CONTAINERS; do
        if remove_resource container "$container" 15 rm -f "$container" >/dev/null; then
            removed_containers="$removed_containers $container"
        fi
    done
    removed_network=""
    if [ "$OWN_NETWORK" = "1" ] &&
        remove_resource network "$NETWORK" 15 network rm "$NETWORK" >/dev/null; then
        removed_network=$NETWORK
    fi
    removed_image=""
    if [ "$OWN_IMAGE" = "1" ] &&
        remove_resource image "$IMAGE" 60 image rm "$IMAGE" >/dev/null; then
        removed_image=$IMAGE
    fi
    removed_volume=""
    if [ "$OWN_VOLUME" = "1" ] &&
        remove_resource volume "$VOLUME" 15 volume rm "$VOLUME" >/dev/null; then
        removed_volume=$VOLUME
    fi
    if [ -n "$removed_containers$removed_network$removed_image$removed_volume" ]; then
        [ -n "$removed_containers" ] && say "removed: containers$removed_containers"
        [ -n "$removed_network" ] && say "removed: network $removed_network"
        [ -n "$removed_image" ] && say "removed: image $removed_image"
        [ -n "$removed_volume" ] && say "removed: volume $removed_volume"
    fi
    if [ -n "$CLEANUP_FAILURES" ]; then
        say "FAIL: cleanup could not remove:$CLEANUP_FAILURES"
        [ "$status" -eq 0 ] && status=1
    fi
    exit "$status"
}

interrupt() {
    say "interrupted; cleaning up this run's resources"
    exit 130
}
