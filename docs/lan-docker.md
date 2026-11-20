# Docker LAN gate: two separate containers, real TLS

The Docker LAN gate is an integration test that runs a real SerialNexus host
and real remote clients in two separate Linux containers on a private bridge
network. It needs no STM32, no serial adapter and no host device. It runs as
its own GitHub Actions job (see [CI](#ci)) and on demand from a clone. It does
not replace the [board gate](board-gate.md), and it does not replace testing on
two real machines behind a real firewall: it proves the software path, not a
network's behaviour.

## What it proves

The two containers exchange nothing except TCP over their own bridge network
and files in a private temporary directory. Inside the host container:

- the real `serial-nexus-terminal` starts the real `serial-nexus-core`;
- a container-local PTY echo device is created and published as
  `/dev/serial/by-id/snx-docker-echo`, which production discovery reads, so
  the core enumerates it as an ordinary serial device;
- sharing is started through the terminal's own UI (Settings checklist, Save,
  the sidebar's Share button), and the pairing keys are taken from the OSC 52
  **copy requests the terminal itself emits**, never from a private back door.

The client container runs the production `RemoteClient` adapter for the human
role and the real `serial-nexus-mcp` binary for the AI role, through the real
client terminal's gateway and a private pairing file.

The gate asserts, in order, and exits non-zero at the first failure:

1. **TLS host authentication and role from credential.** The host certificate
   chain is verified against the session CA, the leaf SHA-256 is pinned, and
   both credentials authenticate; the human key pairs as human and the AI key
   as AI.
2. **Checked-device discovery.** The remote session lists exactly the checked
   device, and its path is the container-local PTY.
3. **Remote history read.** Deterministic device output (`SNX-ECHO READY`) is
   read over the remote session.
4. **Remote human write echo.** A human write comes back as exactly the same
   complete line from the device: the line text must equal the marker, with no
   truncation, reordering or extra bytes. (Serial lines end at `\n` and the
   core trims a trailing `\r`, so this is line semantics, not a raw-dump
   comparison.)
5. **Real MCP process over the remote gateway.** The MCP server lists, connects
   with AI write control and sends bytes that are echoed exactly, through the
   client terminal's gateway.
6. **Human write beside the AI controller.** A human writes while the AI holds
   control, and the AI session sees the human's line.
7. **Invalid credential refused.** A tampered credential fails authentication,
   and the error does not contain the credential.
8. **Wrong host pin refused.** A tampered pinned fingerprint is rejected as a
   host-authentication failure.
9. **ACL revocation.** Unchecking the device on the host immediately removes it
   from the remote listing, refuses remote writes and fails remote MCP reads.
10. **Key rotation rejects old keys.** After the host stops and restarts
    sharing, both old keys are refused; the new human key reads and echoes; the
    real MCP process is restarted with the new AI key and reaches the new
    session.
11. **Owner shutdown disconnects the remote clients.** Quitting the host
    terminal closes the listener and a new TCP connection is refused. The
    human session and a second, independent AI-role connection made with the
    production adapter must both report a typed `Disconnected` result, not a
    timeout or a server error. The real MCP process must still answer a
    device-free request (so a wedged server cannot pass by timing out), and
    its attached device read must fail promptly as a tool-level result rather
    than succeed or time out. The typed probe result, not the MCP error text,
    is the proof that the remote connection is gone. The host container stays
    alive until the client writes its final verdict, so stopping the container
    cannot be the cause of the observed disconnection.

## What it does not prove

- Real USB serial hardware, drivers or firmware; the device is a PTY, not a
  USB adapter.
- A real LAN, router, firewall or NAT. The containers are on a Docker bridge
  network; no traffic leaves it.
- A terminal emulator other than what the gate drives, or the terminal's visual
  layout (other tests cover that).
- Behaviour on platforms other than Linux.

The [board gate](board-gate.md) remains the real-hardware, real-LAN
acceptance procedure.

## Requirements

- Linux with Docker (rootful or rootless). The daemon must support:
  - user-defined **bridge** networks (`docker network create --driver bridge`);
  - an **internal** network (no external route);
  - containers on a `--read-only` root filesystem with a writable `/dev` and
    `/tmp`;
  - PTYs inside the container (`/dev/pts`).
- Roughly 2 GB of free disk for the two pinned images, plus a few hundred MB
  for the build cache.
- For a **cold** image build: access to Docker Hub for the pinned base images
  (`docker pull rust:1.85-bookworm`, then `docker pull debian:12-slim`)
  **and** to crates.io (or a Cargo registry cache), because the build compiles
  the workspace's locked dependencies inside the image. Base images alone are not enough for a cold
  build. Once the image has been built, or when reusing one with
  `--image TAG`, the assertion phase needs no network access: the test network
  is `--internal`, so no container has an external route.
- The Docker CLI on `PATH` (or `DOCKER_CLI` set to it) and permission to use
  the daemon socket. **Docker socket access is equivalent to root on the host**
  for a rootful daemon; run this gate with a daemon you already trust. Rootless
  Docker needs no extra privileges and is what this gate was verified with
  (Docker 29.5.3, rootless, user namespace + slirp4netns).

## Run

From a clone of this repository, at the commit you want gated:

```sh
sh scripts/lan-docker/run.sh
```

The one command builds the pinned image from the workspace, creates the unique
network, runs both containers, prints the gate's `PASS` lines, and removes only
the resources it created. Exit status is `0` only when every assertion passed.

Options:

- `--keep` — leave the containers, network, state volume and image in place
  and print their names (inspect with `docker logs`, then remove them
  yourself). Use only when debugging.
- `--image TAG` — reuse an image already built from this tree instead of
  building; the gate does not remove an image it did not build.
- `--pull` — let the image build refresh its pinned base images.
- `--help` — usage.

Timeouts, all overridable by environment variable:

| Variable | Default | Covers |
| --- | --- | --- |
| `SNX_LAN_BUILD_TIMEOUT` | 1800 s | image build |
| `SNX_LAN_START_TIMEOUT` | 120 s | host terminal start and first sharing session |
| `SNX_LAN_CLIENT_TIMEOUT` | 420 s | every client assertion |
| `SNX_LAN_SHUTDOWN_TIMEOUT` | 30 s | host shutdown and client disconnect |
| `SNX_LAN_DOCKER_TIMEOUT` | 60 s | one ordinary Docker CLI call |

A cached build takes about half a minute; a cold build (pulling the pinned
images) a few minutes. A passing run normally takes under a minute.

## CI

[`.github/workflows/lan-docker.yml`](../.github/workflows/lan-docker.yml) runs
the same command as the job `sh scripts/lan-docker/run.sh` on a GitHub-hosted
`ubuntu-latest` runner:

- it runs on every `pull_request`, on every push to `main`, and on manual
  `workflow_dispatch` (Actions → Docker LAN gate → Run workflow);
- it checks out the event revision and runs
  `sh scripts/lan-docker/run.sh` with no options: a real cold image build from
  the pinned digests, the two isolated containers and the real TLS assertions,
  then the orchestrator's own cleanup. The gate is not mocked, not `--keep`
  and not a prebuilt image, so the job cannot test a stale binary;
- the job fails when the build, any assertion, the cleanup or a phase timeout
  fails, exactly as the command does locally;
- its token is read-only (`permissions: contents: read`), it uses no secret, it
  never uses `pull_request_target`, and it runs on ephemeral GitHub-hosted
  runners, so untrusted pull-request code never reaches a persistent or
  self-hosted daemon;
- the job is bounded to 45 minutes, which covers the documented phase timeouts
  (2370 s) plus Docker-side cleanup. A hung run is killed and fails the job;
- the check is visible on pull requests but is **not a required status check**;
  branch protection is unchanged. Making it required is a separate decision
  once its runtime is known.

The workflow also runs `scripts/lan-docker/tests/run-tests.sh` (the stub-Docker
orchestration harness) and `scripts/lan-docker/tests/workflow-tests.sh` (the
trigger, least-privilege, timeout and exact-command contract for this workflow
and the six checks in `ci.yml`) before the real gate.

CI proves the Actions-hosted Linux path only. It does not prove a real LAN,
firewall, NAT or USB serial hardware, and it does not replace the board gate.
The job log is the gate's own sanitized output: the gate never prints pairing
keys, and a finished job keeps only the runner's ephemeral log, because the
orchestrator removed the containers, network, volume and image it created.

## Isolation

The orchestrator creates, and cleans up:

- **one uniquely named user-defined bridge network** (`snx-lan-<run>-net`),
  created with `--internal` so a container cannot reach the internet or the
  host's LAN;
- **two containers** (`snx-lan-<run>-host`, `snx-lan-<run>-client`) with
  `--cap-drop ALL`, `--security-opt no-new-privileges`, a `--read-only` root
  filesystem, a small writable `/tmp` tmpfs, no published ports (`--publish`
  is never used), no `--network host`, no `--privileged`, no `--device`, and
  no host network or user configuration. After creating each container the
  orchestrator inspects it and fails if any of those properties is wrong.

`<run>` is a timestamp, the shell's PID and four random bytes, so two runs
sharing one daemon (even from different PID namespaces) do not collide. Every
resource is labelled `snx-lan-run=<run>`; before creating anything the
orchestrator refuses a name that already exists rather than adopting it, and
before removing anything in cleanup it checks that label and leaves a foreign
resource in place (reporting that it could not complete the cleanup).

The only writable state the containers share is a **named Docker volume**
(`snx-lan-<run>-state`) mounted at `/state`; it holds the ephemeral pairing
announcement and the step commands. A named volume, rather than a bind mount,
keeps a non-root caller's filesystem permissions from blocking container root
under a rootful daemon, and it is private to the daemon. Each file in it is
created owner-only (`0600`) with a staged rename, and the client refuses a state
file that is not a private regular file. Pairing keys are never passed on a
command line, in an environment variable, or in a log: the host writes them to
that volume, the client reads them, and the host deletes them on shutdown. The
orchestrator never reads the volume's contents; it waits for the host's
non-secret log line that the session and keys exist, and it removes the volume
at the end. All diagnostics are additionally masked for key-like and
credential-like strings.

The simulated device exists only in the host container: the gate creates
`/dev/serial/by-id/snx-docker-echo` as a symlink to its own PTY slave, which is
exactly what production discovery reads. No production discovery override, ACL
shortcut or TLS relaxation is added. The link disappears with the container.

## Images

`scripts/lan-docker/pins.env` pins both images by digest, with the reason and
the refresh procedure next to them:

- build stage `rust:1.85-bookworm` — the workspace's declared Rust minimum,
  and a full toolchain because `ring` (rustls's provider) compiles C and
  assembly;
- runtime stage `debian:12-slim` — only the C library the workspace binaries
  link against. Both are the same Debian release, so the build and runtime C
  libraries match.

The image contains only the workspace's own build output: the core, the
terminal, the MCP server and the gate binary. Building inside the image means a
stale or locally built binary cannot be tested by accident.

## Harness tests

The orchestration itself is tested without a daemon, against a recording
`docker` stub:

```sh
sh scripts/lan-docker/tests/run-tests.sh
```

These cover startup ordering (host before client), failure propagation (build,
missing daemon, failing client), the host ready-marker timeout, scoped cleanup
on normal exit, during a build and on `SIGTERM`, cleanup-failure reporting,
ownership of existing and raced resource names, daemon DNS and generic API
errors during cleanup, diagnostics redaction,
`--keep` on success and failure, and the digest pins. They also assert that no privileged,
published, host-network or device-mapped container is ever created.

The CI workflow's own contract is tested without network or Docker:

```sh
sh scripts/lan-docker/tests/workflow-tests.sh
```

It fails if the workflow loses or adds a trigger (`pull_request` from a fork,
`push` to `main`, `workflow_dispatch`), uses `pull_request_target`, gains a
write scope, a secret, a self-hosted runner or a `paths` filter, stops naming
the job after the command, stops running the exact command with no `--keep`,
`--image` or `--pull`, gains `continue-on-error`, `|| true` or an `if:` skip on
the gate, loses its pinned checkout, or if its timeout stops covering the
timeouts documented above. It also pins the six existing `ci.yml` checks, so
adding this workflow cannot quietly drop or rename one.

## Cleanup and troubleshooting

On success, on any failure, and on `SIGINT`/`SIGTERM`, the orchestrator removes
the two containers, the network, the state volume and the image it built
(unless `--keep`), after checking each one still carries this run's label. A
successful run also waits for the client's completion file before the host
container exits, so the terminal's own cleanup, not container teardown, is what
the shutdown assertions observe. A
removal that fails is reported and makes the run non-zero instead of claiming
the resources are gone. It never runs
`docker system prune`, removes another run's resources, or touches images it
did not build.

- **`cannot reach the Docker daemon`** — start the daemon, fix `DOCKER_HOST`,
  or check that your user can access the socket.
- **`cannot create the private bridge network`** — the daemon does not support
  user-defined bridge or internal networks (some minimal or rootless setups);
  the gate refuses to fall back to host networking.
- **`image build failed`** — the tail of the build log is printed. Usually
  missing network access to Docker Hub, or a full disk.
- **`the host created no announcement within …`** — the host terminal did not
  reach its sharing view. The container logs are printed (redacted); raise
  `SNX_LAN_START_TIMEOUT` on a very slow machine.
- **`client container exited 1`** — a numbered client step failed; the log
  names the step and never prints pairing material.
- **`the test network is not internal`** — the daemon ignored `--internal`;
  stop and investigate rather than weaken the gate.
- **`IPv4 forwarding is disabled` warning** from a rootless daemon is expected
  and harmless: container-to-container traffic on the bridge still works (the
  gate proves it).
- **`FAIL: cleanup could not remove …`** — the daemon refused a removal; the
  listed resources are still there. Remove them by name once the daemon is
  healthy.
- To inspect a failed run, rerun with `--keep` (it keeps resources on failure
  too), use `docker logs`, then remove the printed containers, network and
  volume yourself.

## After the gate

Record the gated commit (`git rev-parse HEAD`), the daemon version and whether
it was rootless, the exit status, and the full `PASS` lines. This gate is tied
to one exact head; if the head changes, run it again.
