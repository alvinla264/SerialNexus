# Board gate: real STM32 + real LAN seam (no flashing)

The coordinator runs this gate on the machine with the existing STM32 test
firmware attached. It exercises the exact PR head over the real serial device
and one real LAN address, and never flashes the board or changes firmware.

## What it proves

1. The core starts beside the normal one on a private socket.
2. A local terminal client opens the real board (local behaviour intact).
3. Sharing starts on one concrete LAN IPv4/IPv6 address; the owner gets a
   session, host pin and human pairing credential, and the terminal's pairing
   view renders two distinct sharing keys that decode to those values.
4. A remote human client pairs over TLS, verifies the host CA and pinned leaf,
   authenticates with the credential bundled in the human sharing key and
   derives its role from it. The older separate fields (address, port, CA,
   fingerprint, credential) remain accepted by the advanced Connect form.
5. Remote discovery lists the checked board, and the remote client joins it.
6. Remote history reads real firmware output; an optional probe can be written
   and an optional firmware echo confirmed.
7. Unchecking revokes the remote read immediately, and local serial access
   still works after sharing stops.

It runs the coordinator's exact-head acceptance for the sharing-key flow as
well: the pairing view's two keys, a human key paired through the normal form,
an old key failing after a sharing restart, and the terminal's MCP pairing
action saving the AI key that the remote MCP gate then uses. It does not test
firmware behaviour beyond what the existing firmware prints or echoes,
NAT/firewalls, or the terminal UI beyond those pairing actions. The
`serial-nexus-mcp` crate adds a companion ignored gate that exercises the actual
MCP-over-stdio pairing-file path against the same existing board echo firmware.

## Prerequisites

- The board is connected and its existing test firmware is running; do not
  flash it. `SER13_BOARD_SERIAL` names its stable path (prefer
  `/dev/serial/by-id/...`).
- The coordinator's user can open that device.
- A concrete LAN address of this machine, for example `192.168.1.20:0`
  (port 0 lets the kernel pick an unused port and the test reports it). Do not
  use `0.0.0.0` or `::`.
- The machine's firewall allows inbound TLS to the chosen port from the remote
  peer; for a pure local seam check, pairing from the same machine over the LAN
  IP already exercises the interface and TLS.
- Port 22/other services are irrelevant; the core binds only the address given.

## Run

Use the coordinator's reviewed local head, never a local edit and never a
fetched PR: this work is merged locally, not through a pull request.

```sh
git rev-parse HEAD                      # must equal the reviewed SHA; record it
git status --short                      # must be empty
nix develop -c cargo build --release --locked
```

Run every gate below on that same SHA. If HEAD changes, repeat the checks,
reviews and gates; record the gated SHA and results before publishing.

Read-only gate (sends nothing to the board):

```sh
nix shell nixpkgs#cargo nixpkgs#rustc -c sh -c '
  SER13_BOARD_SERIAL=/dev/serial/by-id/usb-STMicroelectronics_... \
  SER13_BOARD_BIND=192.168.1.20:0 \
  SER13_BOARD_BAUD=115200 \
  cargo test -p serial-nexus-core --test board_gate -- --ignored --nocapture
'
```

Optional probe and firmware echo (only if the existing firmware echoes it):

```sh
  SER13_BOARD_PROBE=$'\r' \
  SER13_BOARD_EXPECT_ECHO=OK \
  ...
```

Expected lines: `1/7 local open`, `2/7 sharing: bind=...`, `3/7 remote pair`,
`4/7 remote list`, `5/7 remote join`, `6/7 remote read`, `7/7 uncheck revoked
remote access`, then `sharing stopped; local history ... lines`, and finally
`test result: ok. 1 passed`.

Then, on the same exact head, run the terminal-first acceptance below: it uses
the real `serial-nexus-terminal` against a second terminal and the same board,
and the board's new key flow is exercised end to end without printing any key
or credential.

## Terminal-first sharing-key acceptance

This is the gate that covers the new normal pairing experience on real
hardware. On the coordinator's machine:

1. Start the host terminal. In Settings, set a concrete LAN address and port,
   check the board, Save, then start sharing (`s`). Open the pairing view
   (`p`) and confirm it shows the bound address, a **Human terminal key** and a
   distinct **MCP/AI key**, both starting `snx1.`. Press `a` and confirm the
   older host fields (host name, `certificate SHA-256`, CA and two raw
   credentials) are still shown; press `a` again.
2. Start a second `serial-nexus-terminal` on another machine (or another
   user's session). `[ Connect ]`, type the host address and port, then paste
   the whole **human key** in one action and press Enter. Confirm the board
   appears under `Remote <address:port>`, open it and read real firmware
   output. This proves the normal key path, strict role/host checks, TLS CA,
   name and pin verification, and the selected-device ACL.
3. On the host, stop and restart sharing. In the second terminal, try the same
   old human key again: it must fail clearly and must not print the key or
   fall back to an unverified connection. Pair with the new key.
4. In the host terminal's sidebar press `m`, enter the address, port and the
   host's new **MCP/AI key**, and press Enter. Confirm the terminal says the
   MCP pairing was saved and that `~/.config/serial-nexus/mcp-peers.json` is
   mode `0600`. Press `m` again with the same endpoint and confirm that Enter
   asks before replacing and Esc cancels without changing the file.
5. Keep that terminal (which owns the sharing session) running and run the
   terminal-written MCP gate from the same exact head:

   ```sh
   SER18_BOARD_GATEWAY_SOCKET=/run/user/1000/serial-nexus/gateway.sock    SER18_BOARD_PEERS=$HOME/.config/serial-nexus/mcp-peers.json    SER18_BOARD_BIND=192.168.1.20:7331    SER18_BOARD_REMOTE_PATH=/dev/ttyACM0    SER18_BOARD_PROBE=$'hello\r\n'    SER18_BOARD_EXPECT_ECHO=hello    nix develop -c cargo test -p serial-nexus-mcp --test board_gate \
     real_board_terminal_written_pairing_gate -- --ignored --nocapture
   ```

   This gate starts no share and changes nothing: it loads the exact file the
   terminal wrote, reaches the host through the running terminal's gateway,
   lists the checked board, opens it, sends the probe, reads the echo, and
   scans MCP output/stderr for both the AI credential and the AI sharing key.
   Use the remote path the terminal lists (a `/dev/serial/by-id` device may
   appear as `/dev/ttyACM*`).

Do not print, screenshot or paste any key or credential while running this;
the terminal masks key fields and these tests print only endpoints and paths.

## Remote MCP stdio gate

The standalone variant starts its own sharing session. Run it at the same exact
head when a self-contained check is wanted; the terminal-first gate above is
the one that consumes the terminal-written pairing file. This test creates a unique
TemporaryDirectory for the core Unix socket and another for MCP configuration;
the MCP process receives `SERIAL_NEXUS_SOCKET`, `SERIAL_NEXUS_MCP_PEERS`, and
`XDG_CONFIG_HOME` pointing only into those temporary directories. The pairing
file is created with mode `0600` in a mode `0700` directory. The test performs
host open/share setup, remote MCP `list_devices`, `connect`, `send`, and `read`,
checks the firmware echo, unchecks the device, verifies subsequent MCP read is
revoked, and scans tool responses and captured MCP stderr for the sharing keys
and both credentials.
The host uses the canonical path from the local open result for sharing and
checks the canonical path reported by remote discovery; a `/dev/serial/by-id`
input may therefore appear as `/dev/ttyACM*` remotely.
It prints the bound endpoint and board path, never the pairing values.

Set the serial, bind, baud, probe, and expected echo variables as above using
the `SER14_BOARD_*` names (`SER13_BOARD_*` are accepted as aliases). The probe
and expected echo are **required** for this gate: choose only the harmless input
known to work with the board's current echo firmware. There is no flashing or
firmware change. Run:

```sh
SER14_BOARD_SERIAL=/dev/serial/by-id/usb-STMicroelectronics_... \
SER14_BOARD_BIND=192.168.1.20:0 \
SER14_BOARD_BAUD=115200 \
SER14_BOARD_PROBE=$'hello\\r\\n' \
SER14_BOARD_EXPECT_ECHO=hello \
nix develop -c cargo test -p serial-nexus-mcp --test board_gate \
  real_board_remote_mcp_stdio_gate -- --ignored --nocapture
```

Do not run with the ignored flag removed: ordinary workspace tests do not open
a physical board. Record the exact `git rev-parse HEAD` before running; if it
changes, gate the new head separately. The output includes only non-secret
endpoint/path/baud information and pass/fail details.

## If it fails

- `local open` fails: check permissions/cabling; this is not a sharing failure.
- `start sharing` fails: the address is not local or the port is taken; try
  `:0` or another concrete LAN IP. A bind failure must leave sharing off (the
  test would stop there).
- `remote pair` fails: TLS/host-auth or credential failure; record the error.
  Do not weaken certificate checks.
- `remote list`/`join` fails: the board is not checked or the remote credential
  was refused.
- `remote read` is empty: the firmware printed nothing since boot; power-cycle
  the board (never flash), wait for boot output, rerun.
- `probe`/echo fails: the firmware did not echo the probe; report the bytes
  written and the last lines read.
- `uncheck` does not revoke: stop; this is a security failure.

## After the gate

Record: gated SHA, LAN address/port used, board serial path, whether a probe
was sent and its echo, and the test output. The gate is tied to one exact SHA:
if the head changes for any reason, repeat it. Then the coordinator performs
the Conventional Commit squash merge. Do not merge before this gate passes.
