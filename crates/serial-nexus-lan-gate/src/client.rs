//! The client container: the production remote adapter and the real MCP
//! server, across a real TLS connection to the host container.
//!
//! The human role uses `serial_nexus_client::RemoteClient`, the same adapter
//! the human terminal uses. The AI role uses the real `serial-nexus-mcp`
//! process, started the way an AI host starts it, through the real client
//! terminal's gateway and a private pairing file — never a credential in an
//! argument, an environment variable or a log line.

use std::net::{IpAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use serial_nexus_client::{
    save_peer, ClientError, PairingRole, RemoteClient, RemoteEndpoint, WaitSpec,
};
use serial_nexus_protocol::{PairingSecret, SharingKey};

use crate::exchange::{
    acknowledgement_path, announcement_path, command_path, completion_path, ensure_private_dir,
    read_optional_private, remove_if_present, write_private, Acknowledgement, Announcement,
    Command, COMPLETION_OK,
};
use crate::mcp::{Mcp, McpError};
use crate::pty::Tui;
use crate::{note, Binaries};

const STEP: Duration = Duration::from_secs(30);
const ECHO_TIMEOUT_MS: u32 = 8_000;
const DEVICE_BAUD: u32 = 9_600;
const ROWS: u16 = 30;
const COLS: u16 = 100;

/// Numbered steps; a failure names the step that failed and exits non-zero.
struct Steps {
    passed: u32,
}

impl Steps {
    fn new() -> Self {
        Self { passed: 0 }
    }

    fn step<T>(&mut self, name: &str, result: Result<T, String>) -> Result<T, String> {
        match result {
            Ok(value) => {
                self.passed += 1;
                note(&format!("client: PASS {} {}", self.passed, name));
                Ok(value)
            }
            Err(error) => Err(format!("step {} ({name}) failed: {error}", self.passed + 1)),
        }
    }
}

pub fn run(binaries: &Binaries, state: &Path) -> Result<(), String> {
    let work = state.join("client");
    ensure_private_dir(&work).map_err(io_error)?;
    for directory in ["xdg", "home", "run"] {
        ensure_private_dir(&work.join(directory)).map_err(io_error)?;
    }

    let announcement = wait_announcement(state, 1, Duration::from_secs(180))?;
    let endpoint = endpoint_of(&announcement)?;
    let human_key = parse_key(&announcement.human_key, PairingRole::Human)?;
    let ai_key = parse_key(&announcement.ai_key, PairingRole::Ai)?;
    let mut steps = Steps::new();

    // 1. Real separate-container TLS, host authentication and role selection.
    let human = steps.step(
        "TLS host authentication and role from credential",
        (|| -> Result<RemoteClient, String> {
            let human = connect(&endpoint, &human_key, "lan-gate-human")?;
            if human.role() != PairingRole::Human {
                return Err("the human key did not pair as human".to_string());
            }
            let ai = connect(&endpoint, &ai_key, "lan-gate-ai")?;
            if ai.role() != PairingRole::Ai {
                return Err("the AI key did not pair as AI".to_string());
            }
            Ok(human)
        })(),
    )?;

    // 2. The checked device is discovered on the remote session.
    let device = steps.step(
        "checked-device discovery",
        (|| -> Result<String, String> {
            let listed = human
                .list_devices()
                .map_err(|error| format!("remote discovery failed: {error}"))?;
            let paths: Vec<&str> = listed
                .devices
                .iter()
                .map(|device| device.path.as_str())
                .collect();
            if paths.len() != 1 {
                return Err(format!(
                    "expected exactly one checked device, got {paths:?}"
                ));
            }
            if !paths[0].starts_with("/dev/pts/") {
                return Err(format!(
                    "the shared device is not the container-local simulator: {}",
                    paths[0]
                ));
            }
            Ok(paths[0].to_string())
        })(),
    )?;
    note(&format!("client: shared device {device}"));

    // 3. Remote read of deterministic device output.
    let opened = human
        .open(&device, Some(DEVICE_BAUD), false)
        .map_err(|error| format!("cannot open the shared device: {error}"))?;
    steps.step(
        "remote history read",
        read_until_contains(&human, opened.device_id, "SNX-ECHO READY", STEP),
    )?;

    // 4. Remote human write and an exact echo of those bytes.
    let human_marker = format!("PING-HUMAN-{}", nonce());
    steps.step(
        "remote human write echo",
        write_and_match(&human, opened.device_id, &human_marker),
    )?;

    // 5. The real MCP process, through the real client terminal's gateway.
    let terminal_environment = client_terminal_environment(binaries, &work)?;
    let _terminal = Tui::spawn(
        &binaries.terminal,
        &terminal_environment,
        &[
            "SERIAL_NEXUS_SOCKET",
            "SERIAL_NEXUS_GATEWAY_SOCKET",
            "SERIAL_NEXUS_CORE",
            "SERIAL_NEXUS_MCP_PEERS",
            "XDG_CONFIG_HOME",
            "XDG_RUNTIME_DIR",
            "HOME",
        ],
        ROWS,
        COLS,
    )?;
    wait_for_path(&work.join("gateway.sock"), Duration::from_secs(60))?;

    let peers = work.join("peers.json");
    write_peer(&peers, endpoint, &ai_key)?;
    let mut mcp = spawn_mcp(binaries, &work, &peers)?;
    mcp.initialize()?;

    let ai_marker = format!("PING-AI-{}", nonce());
    let ai_device_id = steps.step(
        "real MCP process over the remote gateway",
        (|| -> Result<u64, String> {
            let tools = mcp.list_tools()?;
            let count = tools
                .get("tools")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            if count != 5 {
                return Err(format!("expected 5 MCP tools, got {count}"));
            }

            let listed = mcp
                .call("list_devices", remote_args(&endpoint))
                .map_err(|error| format!("MCP list_devices: {error}"))?;
            if listed.is_error {
                return Err(format!("MCP list_devices failed: {}", listed.text));
            }
            let paths = listed
                .structured
                .get("devices")
                .and_then(serde_json::Value::as_array)
                .map(|devices| {
                    devices
                        .iter()
                        .filter_map(|device| device.get("path").and_then(|path| path.as_str()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if paths != vec![device.as_str()] {
                return Err(format!(
                    "the remote MCP session did not list exactly the checked device: {paths:?}"
                ));
            }

            let mut arguments = remote_args(&endpoint);
            insert(&mut arguments, "path", serde_json::json!(device));
            insert(&mut arguments, "baud", serde_json::json!(DEVICE_BAUD));
            insert(&mut arguments, "request_control", serde_json::json!(true));
            let connected = mcp
                .call("connect", arguments)
                .map_err(|error| format!("MCP connect: {error}"))?;
            if connected.is_error {
                return Err(format!("MCP connect failed: {}", connected.text));
            }
            let control = connected
                .structured
                .get("write_control")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !control {
                return Err("the AI role did not receive write control".to_string());
            }
            let device_id = connected
                .structured
                .get("device_id")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "MCP connect returned no device id".to_string())?;

            let mut arguments = remote_args(&endpoint);
            insert(&mut arguments, "device", serde_json::json!(device_id));
            insert(
                &mut arguments,
                "text",
                serde_json::json!(format!("{ai_marker}\r\n")),
            );
            insert(&mut arguments, "lines", serde_json::json!(1));
            insert(
                &mut arguments,
                "timeout_ms",
                serde_json::json!(ECHO_TIMEOUT_MS),
            );
            let sent = mcp
                .call("send", arguments)
                .map_err(|error| format!("MCP send: {error}"))?;
            if sent.is_error {
                return Err(format!("MCP send failed: {}", sent.text));
            }
            check_mcp_echo(&sent.structured, &ai_marker)?;
            Ok(device_id)
        })(),
    )?;

    // 6. A human writes beside the AI that holds control, and the AI sees it.
    steps.step(
        "human write beside the AI controller",
        (|| -> Result<(), String> {
            let human_marker = format!("PING-HUMAN2-{}", nonce());
            write_and_match(&human, opened.device_id, &human_marker)?;
            let mut arguments = remote_args(&endpoint);
            insert(&mut arguments, "device", serde_json::json!(ai_device_id));
            insert(&mut arguments, "lines", serde_json::json!(200));
            let read = mcp
                .call("read", arguments)
                .map_err(|error| format!("MCP read: {error}"))?;
            if read.is_error || !read.text.contains(&human_marker) {
                return Err("the AI role did not see the human's write".to_string());
            }
            Ok(())
        })(),
    )?;

    // 7. A wrong credential is refused without reaching the device.
    steps.step(
        "invalid credential refused",
        (|| -> Result<(), String> {
            let wrong = tamper_credential(human_key.secret());
            let error = connect_error(&endpoint, &human_key, &wrong)?;
            if !error.contains("authentication failed") {
                return Err(format!("a wrong credential failed differently: {error}"));
            }
            if error.contains(&wrong.credential) {
                return Err("the error echoed the credential".to_string());
            }
            Ok(())
        })(),
    )?;

    // 8. A wrong pinned host certificate is refused.
    steps.step(
        "wrong host pin refused",
        (|| -> Result<(), String> {
            let mut paired = human_key
                .paired_with(endpoint.socket_addr())
                .map_err(|error| error.to_string())?;
            flip_last_hex(&mut paired.host_cert_fingerprint_sha256);
            let error = match RemoteClient::connect_to(
                endpoint,
                &paired,
                human_key.secret(),
                Some("lan-gate-pin".into()),
                STEP,
            ) {
                Ok(_) => return Err("a wrong pinned fingerprint was accepted".to_string()),
                Err(error) => error.to_string(),
            };
            if !error.contains("host authentication failed") {
                return Err(format!("a wrong pin failed differently: {error}"));
            }
            Ok(())
        })(),
    )?;

    // 9. Unchecking revokes the device's remote access immediately.
    steps.step(
        "ACL revocation",
        (|| -> Result<(), String> {
            command(state, 1, "uncheck", STEP)?;
            let listed = human
                .list_devices()
                .map_err(|error| format!("list after revocation: {error}"))?;
            if listed.devices.iter().any(|listed| listed.path == device) {
                return Err("the unchecked device is still listed".to_string());
            }
            if human
                .write_bytes(opened.device_id, b"revoked\r\n", None)
                .is_ok()
            {
                return Err("a revoked device accepted a write".to_string());
            }
            let mut arguments = remote_args(&endpoint);
            insert(&mut arguments, "device", serde_json::json!(ai_device_id));
            let read = mcp
                .call("read", arguments)
                .map_err(|error| format!("MCP read after revocation: {error}"))?;
            if !read.is_error {
                return Err("the revoked device was readable over MCP".to_string());
            }
            Ok(())
        })(),
    )?;

    // 10. Restarting sharing rotates every credential; old keys are refused.
    let mut rotated = steps.step(
        "key rotation rejects old keys",
        (|| -> Result<Rotated, String> {
            command(state, 2, "rotate", STEP)?;
            let second = wait_announcement(state, 2, STEP)?;
            let second_endpoint = endpoint_of(&second)?;
            let second_human = parse_key(&second.human_key, PairingRole::Human)?;
            let second_ai = parse_key(&second.ai_key, PairingRole::Ai)?;

            for key in [&human_key, &ai_key] {
                if connect(&second_endpoint, key, "lan-gate-old").is_ok() {
                    return Err("an old key still paired after rotation".to_string());
                }
            }

            let new_human = connect(&second_endpoint, &second_human, "lan-gate-human-2")?;
            let mut paths: Vec<String> = new_human
                .list_devices()
                .map_err(|error| format!("new session discovery: {error}"))?
                .devices
                .into_iter()
                .map(|device| device.path)
                .collect();
            if paths.len() != 1 {
                return Err(format!("expected one shared device, got {paths:?}"));
            }
            let new_path = paths.pop().expect("one path");
            let opened = new_human
                .open(&new_path, Some(DEVICE_BAUD), false)
                .map_err(|error| format!("new session open: {error}"))?;
            let marker = format!("PING-ROTATED-{}", nonce());
            write_and_match(&new_human, opened.device_id, &marker)?;

            // The real MCP process is restarted with the rotated AI key, so a
            // stale in-memory peer can never stand in for the new session.
            mcp.stop();
            remove_if_present(&peers).map_err(io_error)?;
            write_peer(&peers, second_endpoint, &second_ai)?;
            let mut rotated = spawn_mcp(binaries, &work, &peers)?;
            rotated.initialize()?;
            let listed = rotated
                .call("list_devices", remote_args(&second_endpoint))
                .map_err(|error| format!("rotated MCP list_devices: {error}"))?;
            if listed.is_error || !listed.text.contains(&new_path) {
                return Err("the rotated AI key did not reach the new session".to_string());
            }

            // Attach the rotated MCP session, so the shutdown check later
            // proves its upstream connection drops instead of failing for a
            // device it never opened.
            let mut arguments = remote_args(&second_endpoint);
            insert(&mut arguments, "path", serde_json::json!(new_path.clone()));
            insert(&mut arguments, "baud", serde_json::json!(DEVICE_BAUD));
            insert(&mut arguments, "request_control", serde_json::json!(true));
            let connected = rotated
                .call("connect", arguments)
                .map_err(|error| format!("rotated MCP connect: {error}"))?;
            if connected.is_error {
                return Err(format!(
                    "the rotated AI role could not connect: {}",
                    connected.text
                ));
            }
            let ai_device_id = connected
                .structured
                .get("device_id")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "rotated MCP connect returned no device id".to_string())?;
            let mut arguments = remote_args(&second_endpoint);
            insert(&mut arguments, "device", serde_json::json!(ai_device_id));
            insert(&mut arguments, "lines", serde_json::json!(50));
            let before = rotated
                .call("read", arguments)
                .map_err(|error| format!("rotated MCP read: {error}"))?;
            if before.is_error || !before.text.contains(&marker) {
                return Err(
                    "the rotated MCP session could not read the device before shutdown".to_string(),
                );
            }

            Ok(Rotated {
                endpoint: second_endpoint,
                human: new_human,
                device_id: opened.device_id,
                ai_device_id,
                device_path: new_path,
                rotated_marker: marker,
                ai_key: second_ai,
                mcp: rotated,
            })
        })(),
    )?;

    // 11. Quitting the owner terminal disconnects every remote client.
    steps.step(
        "owner shutdown disconnects the remote client",
        (|| -> Result<(), String> {
            // A second, ordinary AI-role remote connection gives a typed
            // disconnection signal independent of any MCP error text: the
            // production adapter must report `Disconnected`, not a timeout or
            // an unrelated server error.
            let probe = connect(&rotated.endpoint, &rotated.ai_key, "lan-gate-ai-probe")?;
            if probe.role() != PairingRole::Ai {
                return Err("the AI probe did not pair as AI".to_string());
            }
            let probe_device = probe
                .open(&rotated.device_path, Some(DEVICE_BAUD), false)
                .map_err(|error| format!("the AI probe could not open the device: {error}"))?
                .device_id;
            read_until_contains(&probe, probe_device, &rotated.rotated_marker, STEP)?;

            command(state, 3, "quit", STEP)?;
            let deadline = Instant::now() + STEP;
            while rotated.human.is_connected() || probe.is_connected() {
                if Instant::now() >= deadline {
                    return Err("a remote client stayed connected after the owner quit".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if TcpStream::connect_timeout(&rotated.endpoint.socket_addr(), Duration::from_secs(2))
                .is_ok()
            {
                return Err("the listener survived the owner terminal".to_string());
            }
            require_disconnected(
                "the human session",
                rotated.human.read(rotated.device_id, Some(10)),
            )?;
            require_disconnected("the AI-role probe", probe.read(probe_device, Some(10)))?;

            // The real MCP process must still be responsive (a wedged server
            // must not pass by timing out), and its device read must fail as a
            // tool-level result rather than a timeout or a protocol error.
            // The typed probe above is what proves the remote connection is
            // gone; the MCP text itself is not used as that proof.
            let mut arguments = remote_args(&rotated.endpoint);
            insert(
                &mut arguments,
                "device",
                serde_json::json!(rotated.ai_device_id),
            );
            rotated.mcp.timeout = Duration::from_secs(10);
            rotated.mcp.ping().map_err(|error| {
                format!("the MCP server was not responsive after the owner quit: {error}")
            })?;
            classify_shutdown_read(rotated.mcp.call("read", arguments))
        })(),
    )?;

    // Tell the host container it may stop now. Until this file exists the
    // host keeps its core alive, so the disconnections proved above came from
    // the owner terminal's cleanup, not from the container being torn down.
    write_private(&completion_path(state), COMPLETION_OK).map_err(io_error)?;
    rotated.mcp.stop();
    remove_if_present(&peers).map_err(io_error)?;
    note(&format!("client: PASS all {} steps", steps.passed));
    Ok(())
}

struct Rotated {
    endpoint: RemoteEndpoint,
    human: RemoteClient,
    /// The human session's device id, used for its own post-shutdown write.
    device_id: u64,
    /// The rotated MCP session's device id, attached before shutdown so the
    /// post-shutdown failure is a lost connection, not an unopened device.
    ai_device_id: u64,
    /// The rotated device path, a line already in its history, and the AI
    /// key, for the independent typed probe.
    device_path: String,
    rotated_marker: String,
    ai_key: SharingKey,
    mcp: Mcp,
}

fn insert(arguments: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    if let Some(object) = arguments.as_object_mut() {
        object.insert(key.to_string(), value);
    }
}

fn spawn_mcp(binaries: &Binaries, work: &Path, peers: &Path) -> Result<Mcp, String> {
    Mcp::spawn(
        &binaries.mcp,
        &[
            (
                "SERIAL_NEXUS_GATEWAY_SOCKET".to_string(),
                path_text(&work.join("gateway.sock"))?,
            ),
            ("SERIAL_NEXUS_MCP_PEERS".to_string(), path_text(peers)?),
            ("XDG_CONFIG_HOME".to_string(), path_text(&work.join("xdg"))?),
            ("XDG_RUNTIME_DIR".to_string(), path_text(&work.join("run"))?),
            ("HOME".to_string(), path_text(&work.join("home"))?),
        ],
        &["SERIAL_NEXUS_SOCKET", "SERIAL_NEXUS_MCP_PEERS"],
    )
}

fn endpoint_of(announcement: &Announcement) -> Result<RemoteEndpoint, String> {
    let address: IpAddr = announcement
        .address
        .parse()
        .map_err(|_| "the announcement address is not an IP address".to_string())?;
    if announcement.port == 0 {
        return Err("the announcement has no bound port".to_string());
    }
    Ok(RemoteEndpoint::new(address, announcement.port))
}

fn parse_key(text: &str, role: PairingRole) -> Result<SharingKey, String> {
    SharingKey::parse_for(text, role)
        .map_err(|error| format!("the {role:?} sharing key is invalid: {error}"))
}

fn connect(
    endpoint: &RemoteEndpoint,
    key: &SharingKey,
    name: &str,
) -> Result<RemoteClient, String> {
    let paired = key
        .paired_with(endpoint.socket_addr())
        .map_err(|error| format!("cannot pair the key with {endpoint}: {error}"))?;
    RemoteClient::connect_to(
        *endpoint,
        &paired,
        key.secret(),
        Some(name.to_string()),
        STEP,
    )
    .map_err(|error| error.to_string())
}

fn connect_error(
    endpoint: &RemoteEndpoint,
    key: &SharingKey,
    secret: &PairingSecret,
) -> Result<String, String> {
    let paired = key
        .paired_with(endpoint.socket_addr())
        .map_err(|error| error.to_string())?;
    match RemoteClient::connect_to(
        *endpoint,
        &paired,
        secret,
        Some("lan-gate-wrong".into()),
        STEP,
    ) {
        Ok(_) => Err("a wrong credential was accepted".to_string()),
        Err(error) => Ok(error.to_string()),
    }
}

fn tamper_credential(secret: &PairingSecret) -> PairingSecret {
    let mut credential = secret.credential.clone();
    let last = credential.pop().unwrap_or('A');
    credential.push(if last == 'A' { 'B' } else { 'A' });
    PairingSecret {
        role: secret.role,
        credential,
    }
}

fn flip_last_hex(text: &mut String) {
    let replacement = match text.pop() {
        Some('a') => 'b',
        _ => 'a',
    };
    text.push(replacement);
}

/// The write must have completed, written exactly the marker plus CRLF, and
/// produced exactly one complete device line equal to the marker. Additional
/// lines, a partial tail, a timeout or different bytes are not an echo.
fn check_echo(
    lines: &[serial_nexus_client::HistoryLine],
    timed_out: bool,
    bytes_written: usize,
    marker: &str,
) -> Result<(), String> {
    if timed_out {
        return Err(format!(
            "the device did not echo {marker:?} before the wait timed out"
        ));
    }
    if bytes_written != marker.len() + 2 {
        return Err(format!(
            "the write sent {bytes_written} bytes, not the marker plus CRLF ({} bytes)",
            marker.len() + 2
        ));
    }
    if lines.len() != 1 {
        return Err(format!(
            "the device echoed {} line(s) for {marker:?}, not exactly one: {:?}",
            lines.len(),
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
        ));
    }
    let line = &lines[0];
    if line.text != marker || line.partial {
        return Err(format!(
            "the echoed line is not exactly {marker:?} (partial={})",
            line.partial
        ));
    }
    Ok(())
}

/// The MCP send's structured result must show a completed write of exactly the
/// marker plus CRLF, one complete device line equal to the marker, and no
/// timeout, extra line or partial tail.
/// After the owner terminal quits, the MCP read must fail promptly as a
/// tool-level result. This function deliberately does not interpret the tool
/// text as proof of a lost connection: that proof comes from the typed
/// `Disconnected` result of the independent AI-role probe. Here a timeout is a
/// wedged server, a closed stdout means the process died instead of reporting,
/// a protocol error is a broken request, and a successful read is the opposite
/// of a disconnect.
fn classify_shutdown_read(result: Result<crate::mcp::Outcome, McpError>) -> Result<(), String> {
    match result {
        Err(McpError::Closed(error)) => Err(format!(
            "the MCP server closed instead of returning a tool-level failure: {error}"
        )),
        Err(McpError::Timeout(error)) => Err(format!(
            "the MCP read timed out instead of failing promptly: {error}"
        )),
        Err(McpError::Protocol(error)) => Err(format!(
            "the MCP read returned a protocol error instead of a tool result: {error}"
        )),
        Ok(outcome) if outcome.is_error => Ok(()),
        Ok(_) => Err("the MCP session still read after the owner quit".to_string()),
    }
}

/// A request after the owner quit must fail with the adapter's typed
/// `Disconnected`, not a timeout, a server error or a successful read.
fn require_disconnected(
    who: &str,
    result: Result<serial_nexus_protocol::ReadResult, ClientError>,
) -> Result<(), String> {
    match result {
        Err(ClientError::Disconnected(_)) => Ok(()),
        Err(error) => Err(format!(
            "{who} failed after the owner quit for the wrong reason: {error}"
        )),
        Ok(_) => Err(format!("{who} still read after the owner quit")),
    }
}

fn check_mcp_echo(structured: &serde_json::Value, marker: &str) -> Result<(), String> {
    let timed_out = structured
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if timed_out {
        return Err(format!(
            "the AI send timed out before {marker:?} was echoed"
        ));
    }
    let bytes_written = structured
        .get("bytes_written")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    if bytes_written != (marker.len() + 2) as u64 {
        return Err(format!(
            "the AI send wrote {bytes_written} bytes, not the marker plus CRLF"
        ));
    }
    let lines = structured
        .get("lines")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "the AI send returned no lines".to_string())?;
    if lines.len() != 1 {
        return Err(format!(
            "the device echoed {} line(s) for {marker:?}, not exactly one",
            lines.len()
        ));
    }
    let line = &lines[0];
    let text = line
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let partial = line
        .get("partial")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if text != marker || partial {
        return Err(format!(
            "the echoed line is not exactly {marker:?} (partial={partial})"
        ));
    }
    Ok(())
}

fn write_and_match(client: &RemoteClient, device_id: u64, marker: &str) -> Result<(), String> {
    let written = client
        .write_bytes(
            device_id,
            format!("{marker}\r\n").as_bytes(),
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(u64::from(ECHO_TIMEOUT_MS)),
            }),
        )
        .map_err(|error| format!("write failed: {error}"))?;
    check_echo(
        &written.lines,
        written.timed_out,
        written.bytes_written,
        marker,
    )
}

fn read_until_contains(
    client: &RemoteClient,
    device_id: u64,
    needle: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let history = client
            .read(device_id, Some(500))
            .map_err(|error| format!("read failed: {error}"))?;
        if history.lines.iter().any(|line| line.text.contains(needle)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "history never contained {needle:?}; {} line(s) read",
                history.lines.len()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_peer(path: &Path, endpoint: RemoteEndpoint, key: &SharingKey) -> Result<(), String> {
    let paired = key
        .paired_with(endpoint.socket_addr())
        .map_err(|error| error.to_string())?;
    save_peer(path, endpoint, paired, key.secret().clone(), true)
        .map(|_| ())
        .map_err(|error| format!("cannot write the MCP pairing file: {error}"))
}

fn remote_args(endpoint: &RemoteEndpoint) -> serde_json::Value {
    serde_json::json!({
        "ip": endpoint.address.to_string(),
        "port": endpoint.port,
    })
}

fn wait_announcement(
    state: &Path,
    generation: u32,
    timeout: Duration,
) -> Result<Announcement, String> {
    let path = announcement_path(state, generation);
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(bytes) = read_optional_private(&path).map_err(io_error)? {
            return serde_json::from_slice(&bytes)
                .map_err(|error| format!("malformed announcement {generation}: {error}"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the host announced no sharing session {generation} in time"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Write one numbered command and wait for the host's matching answer.
fn command(state: &Path, sequence: u64, name: &str, timeout: Duration) -> Result<(), String> {
    remove_if_present(&acknowledgement_path(state)).map_err(io_error)?;
    let command = Command {
        sequence,
        name: name.to_string(),
    };
    write_private(&command_path(state), command.encode().as_bytes()).map_err(io_error)?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(bytes) =
            read_optional_private(&acknowledgement_path(state)).map_err(io_error)?
        {
            let acknowledgement = Acknowledgement::parse(&bytes)?;
            if acknowledgement.sequence == sequence {
                return match acknowledgement.failure {
                    Some(reason) => Err(format!("the host refused {name}: {reason}")),
                    None => Ok(()),
                };
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("the host did not acknowledge {name} in time"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn client_terminal_environment(
    binaries: &Binaries,
    work: &Path,
) -> Result<Vec<(String, String)>, String> {
    Ok(vec![
        (
            "SERIAL_NEXUS_SOCKET".to_string(),
            path_text(&work.join("core.sock"))?,
        ),
        (
            "SERIAL_NEXUS_GATEWAY_SOCKET".to_string(),
            path_text(&work.join("gateway.sock"))?,
        ),
        ("SERIAL_NEXUS_CORE".to_string(), path_text(&binaries.core)?),
        ("XDG_CONFIG_HOME".to_string(), path_text(&work.join("xdg"))?),
        ("XDG_RUNTIME_DIR".to_string(), path_text(&work.join("run"))?),
        ("HOME".to_string(), path_text(&work.join("home"))?),
    ])
}

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))
}

fn wait_for_path(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the client gateway {} never appeared",
                path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}

fn io_error(error: std::io::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tampered_credential_changes_exactly_one_character() {
        let secret = PairingSecret {
            role: PairingRole::Human,
            credential: "abcdef".to_string(),
        };
        let wrong = tamper_credential(&secret);
        assert_eq!(wrong.role, PairingRole::Human);
        assert_eq!(wrong.credential.len(), secret.credential.len());
        assert_ne!(wrong.credential, secret.credential);
        assert!(!wrong.credential.is_empty());
    }

    #[test]
    fn a_tampered_fingerprint_stays_a_valid_looking_pin() {
        let mut fingerprint = "0".repeat(64);
        flip_last_hex(&mut fingerprint);
        assert_eq!(fingerprint.len(), 64);
        assert_ne!(fingerprint, "0".repeat(64));
        assert!(fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
    }

    fn line(text: &str, partial: bool) -> serial_nexus_client::HistoryLine {
        serial_nexus_client::HistoryLine {
            source: serial_nexus_client::Source::Device,
            text: text.to_string(),
            partial,
        }
    }

    #[test]
    fn an_exact_echo_is_one_complete_matching_line() {
        assert!(check_echo(&[line("M", false)], false, 3, "M").is_ok());
        // Extra output is not the same echo.
        assert!(check_echo(&[line("M", false), line("EXTRA", false)], false, 3, "M").is_err());
        // A partial line is a timeout, not a completed echo.
        assert!(check_echo(&[line("M", true)], false, 3, "M").is_err());
        // Different text, a timeout, or the wrong number of bytes.
        assert!(check_echo(&[line("N", false)], false, 3, "M").is_err());
        assert!(check_echo(&[line("M", false)], true, 3, "M").is_err());
        assert!(check_echo(&[line("M", false)], false, 2, "M").is_err());
    }

    #[test]
    fn an_exact_mcp_echo_rejects_extra_lines_partials_and_timeouts() {
        let exact = serde_json::json!({
            "timed_out": false,
            "bytes_written": 3,
            "lines": [{ "text": "M", "partial": false }],
        });
        assert!(check_mcp_echo(&exact, "M").is_ok());
        let extra = serde_json::json!({
            "timed_out": false,
            "bytes_written": 3,
            "lines": [
                { "text": "M", "partial": false },
                { "text": "EXTRA", "partial": false },
            ],
        });
        assert!(check_mcp_echo(&extra, "M").is_err());
        let partial = serde_json::json!({
            "timed_out": true,
            "bytes_written": 3,
            "lines": [{ "text": "M", "partial": true }],
        });
        assert!(check_mcp_echo(&partial, "M").is_err());
        let missing = serde_json::json!({ "lines": [{ "text": "M", "partial": false }] });
        assert!(check_mcp_echo(&missing, "M").is_err());
    }

    fn mcp_outcome(is_error: bool, text: &str) -> crate::mcp::Outcome {
        crate::mcp::Outcome {
            is_error,
            text: text.to_string(),
            structured: serde_json::Value::Null,
        }
    }

    #[test]
    fn shutdown_mcp_result_must_be_prompt_and_unsuccessful() {
        // A dead MCP process did not report a tool-level failure.
        assert!(classify_shutdown_read(Err(McpError::Closed("gone".into()))).is_err());
        // A timeout is a wedged server, not a prompt failure.
        assert!(classify_shutdown_read(Err(McpError::Timeout("no answer".into()))).is_err());
        assert!(classify_shutdown_read(Err(McpError::Protocol("bad request".into()))).is_err());
        // Any tool-level failure is prompt; the typed probe, not this text,
        // is the disconnection proof.
        assert!(classify_shutdown_read(Ok(mcp_outcome(true, "device not found"))).is_ok());
        assert!(classify_shutdown_read(Ok(mcp_outcome(true, "connection timed out"))).is_ok());
        // A successful read is the opposite of a disconnect.
        assert!(classify_shutdown_read(Ok(mcp_outcome(false, "recent history"))).is_err());
    }

    #[test]
    fn only_a_typed_disconnected_read_proves_a_dropped_connection() {
        assert!(require_disconnected(
            "probe",
            Err(ClientError::Disconnected("remote connection closed".into()))
        )
        .is_ok());
        assert!(
            require_disconnected("probe", Err(ClientError::Timeout("no answer".into()))).is_err()
        );
        assert!(require_disconnected(
            "probe",
            Err(ClientError::Server(serial_nexus_client::ErrorBody::new(
                serial_nexus_client::ErrorCode::NotFound,
                "device not found"
            )))
        )
        .is_err());
        assert!(require_disconnected(
            "probe",
            Ok(serial_nexus_client::ReadResult {
                lines: vec![],
                dropped_lines: 0,
                truncated: false,
            })
        )
        .is_err());
    }

    #[test]
    fn an_announcement_must_name_a_bound_endpoint() {
        let mut announcement = Announcement {
            address: "172.18.0.2".to_string(),
            port: 0,
            human_key: String::new(),
            ai_key: String::new(),
        };
        assert!(endpoint_of(&announcement).is_err());
        announcement.port = 7331;
        assert_eq!(
            endpoint_of(&announcement).expect("endpoint").socket_addr(),
            "172.18.0.2:7331"
                .parse::<std::net::SocketAddr>()
                .expect("addr")
        );
        announcement.address = "not-an-ip".to_string();
        assert!(endpoint_of(&announcement).is_err());
    }
}
