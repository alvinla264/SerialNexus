#!/bin/sh
# Opt-in Docker LAN integration gate for SerialNexus.
#
# One command builds the pinned image, creates a uniquely named private bridge
# network, runs a real SerialNexus host container and a real remote-client
# container, asserts the LAN contract between them, and removes only what it
# created. See docs/lan-docker.md for requirements, timeouts, what it proves
# and troubleshooting.
#
#   sh scripts/lan-docker/run.sh [--keep] [--image TAG] [--pull]
#
# Exit status: 0 only when every assertion passed. 1 on any failure. 130 on
# SIGINT/SIGTERM, after cleaning up.
set -eu

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
script_dir=$repo_root/scripts/lan-docker

KEEP=0
IMAGE_OVERRIDE=""
PULL=0

usage() {
    cat <<'USAGE'
Usage: sh scripts/lan-docker/run.sh [options]

  --keep         leave the containers, network, volume, image and private
                 state in place for inspection (they are named in the output)
  --image TAG    use an image already built from this tree instead of building
  --pull         ask the image build to refresh its pinned base images
  -h, --help     show this help

Environment (all optional):
  DOCKER_CLI                 Docker CLI to run (default: docker)
  SNX_LAN_BUILD_TIMEOUT      image build timeout, seconds (default 1800)
  SNX_LAN_START_TIMEOUT      host sharing-session timeout (default 120)
  SNX_LAN_CLIENT_TIMEOUT     client assertion timeout (default 420)
  SNX_LAN_SHUTDOWN_TIMEOUT   host shutdown timeout (default 30)
  SNX_LAN_DOCKER_TIMEOUT     a single Docker CLI call (default 60)
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1 ;;
        --image)
            [ $# -ge 2 ] || { usage >&2; exit 2; }
                IMAGE_OVERRIDE=$2
            shift
            ;;
        --pull) PULL=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
    shift
done

# The pins are digests with their justification next to them.
set -a
# shellcheck source=pins.env
# shellcheck disable=SC1091
. "$script_dir/pins.env"
set +a

# shellcheck source=lib.sh
# shellcheck disable=SC1091
. "$script_dir/lib.sh"

trap cleanup EXIT
trap interrupt INT TERM

new_names

say "run: $RUN_ID"
say "options: keep=$KEEP image=${IMAGE_OVERRIDE:-built-from-this-tree}"
if head=$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null); then
    say "commit: $head"
    if [ -n "$(git -C "$repo_root" status --porcelain 2>/dev/null)" ]; then
        say "warning: the working tree is dirty; the image reflects it, not only $head"
    fi
fi
say "images: rust $RUST_IMAGE runtime $RUNTIME_IMAGE"
say "timeouts: build ${BUILD_TIMEOUT}s, start ${START_TIMEOUT}s, client ${CLIENT_TIMEOUT}s, shutdown ${SHUTDOWN_TIMEOUT}s"

preflight

if [ "$PULL" = "1" ]; then
    build_image --pull
else
    build_image
fi

create_network
create_volume
create_container host "$HOST_CONTAINER"
create_container client "$CLIENT_CONTAINER"

start_host
start_client
wait_client
wait_host_shutdown

say "PASS: every LAN assertion passed in two separate containers"
