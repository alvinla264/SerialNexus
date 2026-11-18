//! The host container: a real terminal and core, sharing one simulated device.
//!
//! Everything that changes sharing is driven through the terminal's own UI —
//! Settings' device checklist, the Save button, the sidebar's Share button,
//! the pairing view's copy actions, and `Ctrl+Q` to quit — so the gate
//! exercises the supported human path, not a private back door. The pairing
//! keys are taken from the OSC 52 copy requests the terminal itself emits.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use serial_nexus_protocol::SharingKey;

use crate::echo::EchoSim;
use crate::exchange::{
    acknowledgement_path, announcement_path, command_path, completion_path, ensure_private_dir,
    read_optional_private, remove_if_present, write_private, Acknowledgement, Announcement,
    Command, COMPLETION_OK,
};
use crate::pty::{Tui, KEY_CTRL_BRACKET, KEY_CTRL_Q, KEY_CTRL_S, KEY_ESC};
use crate::{note, Binaries};

/// The device's stable name under production's discovery directory.
const DEVICE_NAME: &str = "snx-docker-echo";
const BY_ID_DIR: &str = "/dev/serial/by-id";

const ROWS: u16 = 40;
const COLS: u16 = 120;
const STEP: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(240);
/// Time the host waits, alive, for the client's final verdict after `quit`.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(120);

pub fn run(binaries: &Binaries, state: &Path) -> Result<(), String> {
    let work = state.join("host");
    ensure_private_dir(&work).map_err(io_error)?;
    for directory in ["xdg", "home", "run"] {
        ensure_private_dir(&work.join(directory)).map_err(io_error)?;
    }

    let sim = EchoSim::start(Path::new(BY_ID_DIR), DEVICE_NAME)
        .map_err(|error| format!("cannot start the container-local echo device: {error}"))?;
    note(&format!(
        "host: simulated device {} published as {BY_ID_DIR}/{DEVICE_NAME}",
        sim.path()
    ));

    let address = local_ipv4()?;
    write_settings(&work, &address)?;

    let environment = terminal_environment(binaries, &work)?;
    let mut tui = Tui::spawn(
        &binaries.terminal,
        &environment,
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

    tui.wait_for_text("Devices (", STEP)?;
    tui.wait_for_text(sim.path(), STEP)?;
    focus(&mut tui, "sidebar")?;

    open_settings(&mut tui)?;
    set_device_checked(&mut tui, sim.path(), true)?;
    tui.send(KEY_CTRL_S)?;
    wait_for_settings_applied(&mut tui)?;
    focus(&mut tui, "sidebar")?;
    start_sharing(&mut tui)?;
    let (first, _first_keys) = announce(&mut tui, state, 1)?;
    note(&format!(
        "host: sharing session 1 listening on {}; pairing keys written privately",
        first
    ));

    let mut last = 0u64;
    loop {
        let command = wait_command(state, last, COMMAND_TIMEOUT)?;
        last = command.sequence;
        note(&format!("host: running command {}", command.name));
        let outcome = match command.name.as_str() {
            "uncheck" => uncheck_device(&mut tui, sim.path()),
            "rotate" => rotate(&mut tui, state, sim.path()),
            "quit" => quit(&mut tui, state),
            other => Err(format!("unknown command {other:?}")),
        };
        match outcome {
            Ok(message) => {
                write_private(
                    &acknowledgement_path(state),
                    Acknowledgement::ok(command.sequence).as_bytes(),
                )
                .map_err(io_error)?;
                if let Some(message) = message {
                    note(&message);
                }
                if command.name == "quit" {
                    // The terminal has quit, but keep this container (and its
                    // core) alive until the client has finished its shutdown
                    // assertions. Otherwise stopping the container would be
                    // an alternative cause for the very disconnection the
                    // client is checking.
                    note("host: terminal quit; waiting for the client's final verdict");
                    wait_for_completion(state, COMPLETION_TIMEOUT)?;
                    return Ok(());
                }
            }
            Err(message) => {
                let _ = write_private(
                    &acknowledgement_path(state),
                    Acknowledgement::failed(command.sequence, &message).as_bytes(),
                );
                return Err(format!("command {} failed: {message}", command.name));
            }
        }
    }
}

fn terminal_environment(binaries: &Binaries, work: &Path) -> Result<Vec<(String, String)>, String> {
    let text = |path: &Path| {
        path.to_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{} is not UTF-8", path.display()))
    };
    Ok(vec![
        ("SERIAL_NEXUS_SOCKET".into(), text(&work.join("core.sock"))?),
        (
            "SERIAL_NEXUS_GATEWAY_SOCKET".into(),
            text(&work.join("gateway.sock"))?,
        ),
        ("SERIAL_NEXUS_CORE".into(), text(&binaries.core)?),
        ("XDG_CONFIG_HOME".into(), text(&work.join("xdg"))?),
        ("XDG_RUNTIME_DIR".into(), text(&work.join("run"))?),
        ("HOME".into(), text(&work.join("home"))?),
    ])
}

/// The terminal's own settings file: a concrete container address and port 0,
/// so the kernel picks the port and the test proves the bound one.
fn write_settings(work: &Path, address: &str) -> Result<(), String> {
    let directory = work.join("xdg").join("serial-nexus");
    ensure_private_dir(&directory).map_err(io_error)?;
    let settings = serde_json::json!({
        "local_echo": true,
        "scrollback": true,
        "share_address": address,
        "share_port": 0,
    });
    let mut bytes = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    write_private(&directory.join("terminal.json"), &bytes).map_err(io_error)
}

fn local_ipv4() -> Result<String, String> {
    use nix::ifaddrs::getifaddrs;

    let addresses = getifaddrs().map_err(|error| format!("cannot list interfaces: {error}"))?;
    for interface in addresses {
        let Some(address) = interface.address else {
            continue;
        };
        let Some(inet) = address.as_sockaddr_in() else {
            continue;
        };
        let ip = inet.ip();
        if ip.is_loopback() || ip.is_unspecified() {
            continue;
        }
        return Ok(ip.to_string());
    }
    Err("the container has no private IPv4 address".to_string())
}

/// Move the keyboard focus until the footer names `want`.
fn focus(tui: &mut Tui, want: &str) -> Result<(), String> {
    let footer = format!("focus: {want}");
    let deadline = Instant::now() + STEP;
    loop {
        if tui.text().contains(&footer) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("the terminal never focused {want}"));
        }
        tui.send(KEY_CTRL_BRACKET)?;
        std::thread::sleep(Duration::from_millis(120));
    }
}

fn click_text(tui: &mut Tui, needle: &str) -> Result<(), String> {
    let (column, row) = tui
        .find(needle)
        .ok_or_else(|| format!("{needle:?} is not on the terminal screen"))?;
    tui.click(column, row)
}

fn open_settings(tui: &mut Tui) -> Result<(), String> {
    tui.click(3, tui.rows().saturating_sub(3))?;
    tui.wait_for_text("focus: settings", STEP)
}

/// Click one device's checkbox in the Settings pane, from `from` to `to`.
fn set_device_checked(tui: &mut Tui, path: &str, checked: bool) -> Result<(), String> {
    let (from, to) = if checked {
        ("[ ]", "[x]")
    } else {
        ("[x]", "[ ]")
    };
    let row = (0..tui.rows())
        .find(|row| {
            let line = tui.row(*row);
            line.contains(from) && line.contains(path)
        })
        .ok_or_else(|| format!("no unchecked row for {path} in Settings"))?;
    let column = tui
        .row(row)
        .find(path)
        .ok_or_else(|| "the device row lost its path".to_string())? as u16;
    tui.click(column, row)?;
    let path = path.to_string();
    tui.wait_until("the device checkbox to change", STEP, |text| {
        text.lines()
            .any(|line| line.contains(to) && line.contains(&path))
    })
}

/// Saving while sharing is on applies the checklist and may replace the
/// "settings saved" notice with a "no longer shared" one in the same frame, so
/// either is a successful save. A failed save leaves its own error notice.
fn wait_for_settings_applied(tui: &mut Tui) -> Result<(), String> {
    tui.wait_until("the settings save to be applied", STEP, |text| {
        text.contains("settings saved") || text.contains("no longer shared")
    })
}

fn start_sharing(tui: &mut Tui) -> Result<(), String> {
    click_text(tui, "[ Share: off ]")?;
    tui.wait_for_text("focus: pairing", STEP)?;
    tui.wait_for_text("Human terminal key", STEP)
}

fn stop_sharing(tui: &mut Tui) -> Result<(), String> {
    click_text(tui, "[ Share: on ]")?;
    tui.wait_for_text("sharing stopped", STEP)
}

/// Wait for the pairing view's bound endpoint, copy both keys from the
/// terminal's own OSC 52 requests, and write one private announcement.
fn announce(
    tui: &mut Tui,
    state: &Path,
    generation: u32,
) -> Result<(SocketAddr, (String, String)), String> {
    let bind = wait_for_bind(tui)?;
    let transcript = tui.raw_transcript().len();
    tui.send_text("c")?;
    let human = wait_for_copied_key(tui, "snx1.human.", transcript)?;
    let transcript = tui.raw_transcript().len();
    tui.send_text("C")?;
    let ai = wait_for_copied_key(tui, "snx1.ai.", transcript)?;

    // Validate before publishing: a key the production parser would reject
    // must fail the gate, not be handed to the client.
    SharingKey::parse_for(&human, serial_nexus_protocol::PairingRole::Human)
        .map_err(|error| format!("the terminal copied an invalid human key: {error}"))?;
    SharingKey::parse_for(&ai, serial_nexus_protocol::PairingRole::Ai)
        .map_err(|error| format!("the terminal copied an invalid AI key: {error}"))?;

    let announcement = Announcement {
        address: bind.ip().to_string(),
        port: bind.port(),
        human_key: human.clone(),
        ai_key: ai.clone(),
    };
    let bytes = serde_json::to_vec(&announcement).map_err(|error| error.to_string())?;
    write_private(&announcement_path(state, generation), &bytes).map_err(io_error)?;
    tui.send_text("x")?;
    tui.wait_for_text("Human terminal key", STEP)?;
    Ok((bind, (human, ai)))
}

fn wait_for_bind(tui: &mut Tui) -> Result<SocketAddr, String> {
    let deadline = Instant::now() + STEP;
    loop {
        if let Some(bind) = parse_bind(&tui.text()) {
            return Ok(bind);
        }
        if Instant::now() >= deadline {
            return Err("the pairing view never showed the bound endpoint".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The pairing view's first line is `Sharing on <address>:<port> · …`.
fn parse_bind(text: &str) -> Option<SocketAddr> {
    let line = text.lines().find(|line| line.contains("Sharing on "))?;
    let rest = line.split_once("Sharing on ")?.1;
    rest.split_whitespace().next()?.parse().ok()
}

/// Every complete key the TUI asked the terminal to copy, from OSC 52.
fn copied_keys(transcript: &[u8]) -> Vec<String> {
    use base64::Engine as _;
    let start = b"\x1b]52;c;";
    let end = b"\x1b\\";
    let mut keys = Vec::new();
    let mut offset = 0usize;
    while let Some(found) = transcript[offset..]
        .windows(start.len())
        .position(|window| window == start)
    {
        let payload_start = offset + found + start.len();
        let Some(rest) = transcript
            .get(payload_start..)
            .and_then(|rest| rest.windows(end.len()).position(|window| window == end))
        else {
            break;
        };
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD
            .decode(&transcript[payload_start..payload_start + rest])
        {
            if let Ok(key) = String::from_utf8(bytes) {
                keys.push(key);
            }
        }
        offset = payload_start + rest + end.len();
    }
    keys
}

fn wait_for_copied_key(tui: &mut Tui, prefix: &str, since: usize) -> Result<String, String> {
    let deadline = Instant::now() + STEP;
    loop {
        let transcript = tui.raw_transcript();
        if let Some(key) = copied_keys(&transcript[since.min(transcript.len())..])
            .into_iter()
            .find(|key| key.starts_with(prefix))
        {
            return Ok(key);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the terminal never offered a {prefix} key for copying"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Leave the pairing view for the live stream, so the sidebar's buttons and
/// the settings checklist are the next things clicked.
fn leave_pairing(tui: &mut Tui) -> Result<(), String> {
    if tui.text().contains("focus: pairing") {
        tui.send(KEY_ESC)?;
        tui.wait_until("the live stream", STEP, |text| {
            text.contains("focus: stream")
        })?;
    }
    Ok(())
}

fn uncheck_device(tui: &mut Tui, path: &str) -> Result<Option<String>, String> {
    leave_pairing(tui)?;
    focus(tui, "sidebar")?;
    open_settings(tui)?;
    set_device_checked(tui, path, false)?;
    tui.send(KEY_CTRL_S)?;
    wait_for_settings_applied(tui)?;
    Ok(Some(
        "host: device unchecked; sharing stays on with no checked device".to_string(),
    ))
}

fn rotate(tui: &mut Tui, state: &Path, path: &str) -> Result<Option<String>, String> {
    // The sharing view is still open from session 1. Leave it, stop the
    // session (fresh CA, leaf and credentials next time), re-check the device
    // and start again.
    leave_pairing(tui)?;
    focus(tui, "sidebar")?;
    stop_sharing(tui)?;
    tui.wait_for_text("[ Share: off ]", STEP)?;
    open_settings(tui)?;
    set_device_checked(tui, path, true)?;
    tui.send(KEY_CTRL_S)?;
    wait_for_settings_applied(tui)?;
    focus(tui, "sidebar")?;
    start_sharing(tui)?;
    let (bind, _) = announce(tui, state, 2)?;
    Ok(Some(format!(
        "host: sharing restarted on {bind}; previous keys are revoked"
    )))
}

fn quit(tui: &mut Tui, state: &Path) -> Result<Option<String>, String> {
    // Leave the pairing view and quit through the terminal's cleanup path.
    leave_pairing(tui)?;
    focus(tui, "sidebar")?;
    tui.send(KEY_CTRL_Q)?;
    let status = tui.wait_exit(Duration::from_secs(15))?;
    if !status.success() {
        return Err(format!("the terminal exited with {status}"));
    }
    if tui.in_alternate_screen() {
        return Err("the terminal never left the alternate screen".to_string());
    }
    remove_if_present(&announcement_path(state, 1)).map_err(io_error)?;
    remove_if_present(&announcement_path(state, 2)).map_err(io_error)?;
    Ok(Some(
        "host: the owner terminal exited and the listener is gone".to_string(),
    ))
}

/// Wait for the client's completion file. A missing or negative verdict is a
/// failure, never a silent pass.
fn wait_for_completion(state: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(bytes) = read_optional_private(&completion_path(state)).map_err(io_error)? {
            if bytes == COMPLETION_OK {
                return Ok(());
            }
            return Err("the client reported that its shutdown assertions failed".to_string());
        }
        if Instant::now() >= deadline {
            return Err("the client did not report its shutdown verdict in time".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_command(state: &Path, after: u64, timeout: Duration) -> Result<Command, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(bytes) = read_optional_private(&command_path(state)).map_err(io_error)? {
            let command = Command::parse(&bytes)?;
            if command.sequence > after {
                return Ok(command);
            }
        }
        if Instant::now() >= deadline {
            return Err("the client sent no command in time".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn io_error(error: std::io::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "snx1.human.eyJ2IjoxfQ";

    fn osc52(payload: &str) -> Vec<u8> {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        format!("\x1b]52;c;{encoded}\x1b\\").into_bytes()
    }

    #[test]
    fn copies_are_read_only_from_the_new_transcript_and_whole() {
        let mut transcript = osc52("snx1.human.old");
        let since = transcript.len();
        transcript.extend(osc52(KEY));
        transcript.extend(b"\x1b]52;c;!!!not-base64!!!\x1b\\");
        assert_eq!(copied_keys(&transcript[since..]), vec![KEY.to_string()]);
    }

    #[test]
    fn the_bound_endpoint_is_read_from_the_pairing_line() {
        let screen = "Sharing on 172.18.0.2:43199 · 1 remote client(s) · 0 connecting\n\
                      Firewall and router settings decide who can reach this address";
        assert_eq!(
            parse_bind(screen),
            Some("172.18.0.2:43199".parse().expect("addr"))
        );
        assert_eq!(parse_bind("nothing here"), None);
        assert_eq!(parse_bind("Sharing on not-an-address · 0"), None);
    }

    #[test]
    fn completion_must_arrive_and_say_all_passed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let state = directory.path().join("state");
        ensure_private_dir(&state).expect("private dir");
        assert!(wait_for_completion(&state, Duration::from_millis(10)).is_err());
        write_private(&completion_path(&state), b"failed\n").expect("write");
        assert!(wait_for_completion(&state, Duration::from_millis(10)).is_err());
        write_private(&completion_path(&state), COMPLETION_OK).expect("write");
        assert!(wait_for_completion(&state, Duration::from_millis(10)).is_ok());
    }

    #[test]
    fn a_device_checkbox_is_not_confused_with_the_sidebar_row() {
        // The sidebar shows the path without a checkbox; the settings pane's
        // row is the one this helper clicks. This pins the distinguishing mark.
        let sidebar = " ▸ /dev/pts/3";
        let settings_unchecked = "  [ ] /dev/pts/3";
        assert!(!sidebar.contains("[ ]"));
        assert!(settings_unchecked.contains("[ ]"));
    }
}
