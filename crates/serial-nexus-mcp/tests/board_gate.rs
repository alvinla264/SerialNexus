//! Ignored, never-flashing STM32 acceptance gate for the real remote MCP path.
//!
//! Uses the already-attached board and its existing echo firmware. The host
//! core uses a unique temporary Unix socket; the MCP child gets a private
//! temporary config directory and owner-only pairing file. No pairing data is
//! printed. See `docs/board-gate.md` for the coordinator instructions.

mod support;

use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use serial_nexus_client::{
    Client, ClientInfo, GatewayConfig, GatewayServer, Peers, RemoteEndpoint,
};
use serial_nexus_protocol::{PairingSecret, ShareBind, SharingKey};
use support::{start_core, McpSession};
use tempfile::TempDir;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn write_private_pairing_file(
    directory: &TempDir,
    address: SocketAddr,
    secret: &PairingSecret,
    paired: &serial_nexus_protocol::PairedHost,
) -> PathBuf {
    let config = directory.path().join("config");
    std::fs::create_dir(&config).expect("create isolated MCP config directory");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700))
        .expect("protect MCP config directory");
    let path = config.join("mcp-peers.json");
    let content = json!({
        "version": 1,
        "peers": [{
            "endpoint": { "address": address.ip().to_string(), "port": address.port() },
            "paired": paired,
            "secret": { "role": secret.role, "credential": secret.credential },
        }],
    });
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create private MCP pairing file");
    file.write_all(content.to_string().as_bytes())
        .expect("write pairing file");
    file.sync_all().expect("sync pairing file");
    path
}

fn remote_args(address: SocketAddr, extra: Value) -> Value {
    let mut object = extra.as_object().cloned().unwrap_or_default();
    object.insert("ip".into(), json!(address.ip().to_string()));
    object.insert("port".into(), json!(address.port()));
    Value::Object(object)
}

fn assert_no_secret(secret: &PairingSecret, output: &str, stderr: &str) {
    assert!(
        !output.contains(&secret.credential),
        "pairing credential leaked in MCP output"
    );
    assert!(
        !stderr.contains(&secret.credential),
        "pairing credential leaked in MCP stderr"
    );
}

#[test]
#[ignore = "needs an attached STM32 running existing echo firmware and a concrete LAN bind; see docs/board-gate.md"]
fn real_board_remote_mcp_stdio_gate() {
    let serial = env("SER14_BOARD_SERIAL")
        .or_else(|| env("SER13_BOARD_SERIAL"))
        .expect("set SER14_BOARD_SERIAL (or SER13_BOARD_SERIAL) to the existing board serial path");
    let bind: SocketAddr = env("SER14_BOARD_BIND")
        .or_else(|| env("SER13_BOARD_BIND"))
        .expect("set SER14_BOARD_BIND (or SER13_BOARD_BIND) to one concrete LAN <ip>:<port>")
        .parse()
        .expect("board bind must be <ip>:<port>");
    assert!(
        !bind.ip().is_unspecified(),
        "bind a concrete LAN address, not a wildcard"
    );
    let baud: u32 = env("SER14_BOARD_BAUD")
        .or_else(|| env("SER13_BOARD_BAUD"))
        .unwrap_or_else(|| "115200".into())
        .parse()
        .expect("board baud must be a number");
    let probe = env("SER14_BOARD_PROBE")
        .or_else(|| env("SER13_BOARD_PROBE"))
        .expect("set SER14_BOARD_PROBE to a harmless line accepted by the existing echo firmware");
    let expected_echo = env("SER14_BOARD_EXPECT_ECHO")
        .or_else(|| env("SER13_BOARD_EXPECT_ECHO"))
        .expect("set SER14_BOARD_EXPECT_ECHO to the expected firmware echo");

    // Both locations are fresh temp paths. The MCP child is explicitly pinned
    // to this host's core and cannot autostart a default/user core.
    let (core_directory, socket) = start_core(Duration::from_secs(30));
    let gateway_socket = core_directory.path().join("gateway.sock");
    let _gateway = GatewayServer::start(GatewayConfig {
        socket: gateway_socket.clone(),
        core_socket: socket.clone(),
    })
    .expect("start gateway for this board-gate core");
    let owner = Client::connect_to_socket(&socket, ClientInfo::human("board-gate-owner"))
        .expect("local owner connects");
    let local = owner
        .open(&serial, Some(baud), false)
        .expect("locally open existing board firmware (no flashing)");
    // The board may have been selected through /dev/serial/by-id or another
    // symlink. The core reports its canonical node path; that is the stable
    // path returned by remote discovery and accepted by remote tools.
    let canonical = local.port.clone();
    let pairing = owner
        .start_sharing(
            ShareBind::new(bind.ip(), bind.port()),
            vec![canonical.clone()],
        )
        .expect("opt in to sharing the checked board");
    let address = pairing.bind;

    let mcp_config = TempDir::new().expect("isolated MCP configuration");
    let peers = write_private_pairing_file(&mcp_config, address, &pairing.ai, &pairing.host);
    let peers_text = peers.to_string_lossy().into_owned();
    let config_text = mcp_config.path().to_string_lossy().into_owned();
    let mut mcp = McpSession::spawn_at_gateway(
        &socket,
        &gateway_socket,
        &[
            ("SERIAL_NEXUS_MCP_PEERS", &peers_text),
            ("XDG_CONFIG_HOME", &config_text),
        ],
    );
    let list = mcp.call_tool("list_devices", remote_args(address, json!({})));
    assert!(!list.is_error, "remote list failed: {}", list.text);
    assert!(
        list.text.contains(&canonical),
        "canonical shared board path absent: {}",
        list.text
    );
    assert!(
        list.text.contains(&address.to_string()),
        "endpoint absent: {}",
        list.text
    );

    let connected = mcp.call_tool(
        "connect",
        remote_args(
            address,
            json!({ "path": canonical.clone(), "request_control": true }),
        ),
    );
    assert!(
        !connected.is_error,
        "remote connect failed: {}",
        connected.text
    );
    let device_id = connected.device_id();

    let sent = mcp.call_tool(
        "send",
        remote_args(
            address,
            json!({
                "device": device_id,
                "text": probe.clone(),
                "lines": 1,
                "timeout_ms": 5000,
            }),
        ),
    );
    assert!(!sent.is_error, "remote send failed: {}", sent.text);
    assert!(
        sent.text.contains(&expected_echo),
        "expected echo {expected_echo:?} absent: {}",
        sent.text
    );

    let read = mcp.call_tool(
        "read",
        remote_args(address, json!({ "device": device_id, "lines": 20 })),
    );
    assert!(!read.is_error, "remote read failed: {}", read.text);
    assert!(
        read.text.contains(&expected_echo),
        "echo absent from remote history: {}",
        read.text
    );

    // Revocation is tested over the same actual MCP stdio path. The remote
    // client may still have a cached transport, but can no longer use the node.
    owner
        .uncheck_shared_devices(vec![canonical.clone()])
        .expect("owner revokes checked device");
    let revoked = mcp.call_tool(
        "read",
        remote_args(address, json!({ "device": device_id, "lines": 1 })),
    );
    assert!(
        revoked.is_error,
        "MCP read after revocation unexpectedly worked: {}",
        revoked.text
    );

    let output = format!(
        "{}\n{}\n{}\n{}",
        list.text, connected.text, sent.text, read.text
    );
    // The helper captures stderr incrementally on another thread.
    std::thread::sleep(Duration::from_millis(50));
    let stderr = mcp.stderr_text();
    assert_no_secret(&pairing.ai, &output, &stderr);
    assert_no_secret(&pairing.human, &output, &stderr);
    assert_no_secret(&pairing.ai, &revoked.text, &stderr);
    assert_no_secret(&pairing.human, &revoked.text, &stderr);
    // The new single-string keys must not appear either.
    for secret in [&pairing.ai, &pairing.human] {
        let key = SharingKey::encode(&pairing.host, secret).expect("session key encodes");
        assert!(!output.contains(&key), "a sharing key leaked in MCP output");
        assert!(!stderr.contains(&key), "a sharing key leaked in MCP stderr");
        assert!(
            !revoked.text.contains(&key),
            "a sharing key leaked in MCP output"
        );
    }

    owner.stop_sharing().expect("stop sharing");
    owner.close(local.device_id).ok();
    drop(mcp);
    drop(owner);
    drop(core_directory);
    eprintln!("remote MCP board gate passed: endpoint={address}, canonical board path={canonical}, baud={baud}; pairing values were not printed");
}

/// The coordinator's terminal-first acceptance: a real foreground terminal owns
/// the sharing session and wrote the AI key through its MCP pairing action.
/// This gate only consumes that file and endpoint; it starts no share and never
/// changes sharing or unchecks the board.
#[test]
#[ignore = "needs a running terminal that owns a real STM32 share and its terminal-written pairing file; see docs/board-gate.md"]
fn real_board_terminal_written_pairing_gate() {
    let gateway = env("SER18_BOARD_GATEWAY_SOCKET")
        .expect("set SER18_BOARD_GATEWAY_SOCKET to the running terminal's gateway socket");
    let peers_path = env("SER18_BOARD_PEERS")
        .expect("set SER18_BOARD_PEERS to the terminal-written mcp-peers.json path");
    let endpoint: SocketAddr = env("SER18_BOARD_BIND")
        .expect("set SER18_BOARD_BIND to the host's <ip>:<port> shown in the pairing view")
        .parse()
        .expect("SER18_BOARD_BIND must be <ip>:<port>");
    let path = env("SER18_BOARD_REMOTE_PATH")
        .expect("set SER18_BOARD_REMOTE_PATH to the checked board's path as the terminal lists it");
    let probe = env("SER18_BOARD_PROBE")
        .or_else(|| env("SER14_BOARD_PROBE"))
        .expect("set SER18_BOARD_PROBE to a harmless line accepted by the existing echo firmware");
    let expected_echo = env("SER18_BOARD_EXPECT_ECHO")
        .or_else(|| env("SER14_BOARD_EXPECT_ECHO"))
        .expect("set SER18_BOARD_EXPECT_ECHO to the expected firmware echo");

    // The file the terminal's `m` action wrote must load under the same rules
    // MCP uses, and must pair exactly the endpoint being dialled.
    let peers = Peers::load(&peers_path).expect("load the terminal-written pairing file");
    let wanted = RemoteEndpoint::from_socket_addr(endpoint).canonical();
    let peer = peers
        .find(&wanted)
        .expect("the terminal-written file pairs the host endpoint");
    let credential = peer.secret.credential.clone();
    let ai_key = SharingKey::encode(&peer.paired, &peer.secret)
        .expect("the saved AI pairing forms a sharing key");

    // The MCP child reaches the host through the running terminal's gateway;
    // the core socket is deliberately absent because MCP only uses the gateway.
    let mut mcp = McpSession::spawn_at_gateway(
        Path::new("/nonexistent-board-gate-core.sock"),
        Path::new(&gateway),
        &[("SERIAL_NEXUS_MCP_PEERS", &peers_path)],
    );
    let list = mcp.call_tool("list_devices", remote_args(endpoint, json!({})));
    assert!(!list.is_error, "remote list failed: {}", list.text);
    assert!(
        list.text.contains(&path),
        "the shared board path {path:?} is absent: {}",
        list.text
    );
    assert!(
        list.text.contains(&endpoint.to_string()),
        "endpoint absent: {}",
        list.text
    );

    let connected = mcp.call_tool(
        "connect",
        remote_args(
            endpoint,
            json!({ "path": path.clone(), "request_control": true }),
        ),
    );
    assert!(
        !connected.is_error,
        "remote connect failed: {}",
        connected.text
    );
    let device_id = connected.device_id();
    let sent = mcp.call_tool(
        "send",
        remote_args(
            endpoint,
            json!({
                "device": device_id,
                "text": probe,
                "lines": 1,
                "timeout_ms": 5000,
            }),
        ),
    );
    assert!(!sent.is_error, "remote send failed: {}", sent.text);
    assert!(
        sent.text.contains(&expected_echo),
        "expected echo {expected_echo:?} absent: {}",
        sent.text
    );
    let read = mcp.call_tool(
        "read",
        remote_args(endpoint, json!({ "device": device_id, "lines": 20 })),
    );
    assert!(!read.is_error, "remote read failed: {}", read.text);
    assert!(
        read.text.contains(&expected_echo),
        "echo absent from remote history: {}",
        read.text
    );

    std::thread::sleep(Duration::from_millis(50));
    let stdout = format!(
        "{}\n{}\n{}\n{}",
        list.text, connected.text, sent.text, read.text
    );
    let stderr = mcp.stderr_text();
    assert!(
        !stdout.contains(&credential) && !stderr.contains(&credential),
        "the terminal-written AI credential leaked through MCP"
    );
    assert!(
        !stdout.contains(&ai_key) && !stderr.contains(&ai_key),
        "the terminal-written AI sharing key leaked through MCP"
    );

    // The terminal owns the session: this gate must not stop sharing, uncheck
    // the board, or change the file.
    drop(mcp);
    eprintln!(
        "terminal-written MCP board gate passed: endpoint={endpoint}, board path={path:?}; no pairing value was printed"
    );
}
