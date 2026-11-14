//! The normal pairing experience against real cores: one copy-and-paste key
//! for a human terminal, one for MCP/AI, strict role and host checks, rotation,
//! and the safe save of the owner-only MCP pairing file.
//!
//! Nothing here writes a secret to the settings file, a log or a device, and a
//! failed key check never dials the host.

mod support;

use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serial_nexus_client::{Client, ClientInfo, ShareBind};
use serial_nexus_protocol::{PairingRole, ShareSessionInfo, SharingKey};
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::input::Key;
use serial_nexus_terminal::settings::SettingsStore;
use serial_nexus_terminal::state::{FormMode, View};
use support::{connect, start_core, PortListerGuard, SimulatedDevice};
use tempfile::TempDir;

static KEY_TESTS: Mutex<()> = Mutex::new(());

fn serial_test() -> MutexGuard<'static, ()> {
    KEY_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct Host {
    _core: TempDir,
    owner: Client,
    info: ShareSessionInfo,
    _lister: PortListerGuard,
}

fn start_host(device: &SimulatedDevice) -> Host {
    let (core, socket) = start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let lister = PortListerGuard::from_path(device.path.clone());
    let owner = connect(&socket, ClientInfo::human("host-terminal"));
    let info = owner
        .start_sharing(
            ShareBind::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            vec![device.path.clone()],
        )
        .expect("sharing starts");
    Host {
        _core: core,
        owner,
        info,
        _lister: lister,
    }
}

struct Terminal {
    app: App,
    peers: PathBuf,
    _xdg: TempDir,
    _peers_dir: TempDir,
}

impl Terminal {
    fn new(socket: &PathBuf) -> Self {
        let xdg = TempDir::new().expect("xdg");
        let settings_path = xdg.path().join("serial-nexus/terminal.json");
        let settings = SettingsStore::at(&settings_path)
            .load_or_create()
            .expect("settings");
        let client = connect(socket, ClientInfo::human("key-terminal"));
        let mut app = App::with_preferences(
            client,
            40,
            110,
            settings,
            Some(SettingsStore::at(&settings_path)),
        );
        let peers_dir = TempDir::new().expect("peers dir");
        let peers = peers_dir.path().join("mcp-peers.json");
        app.set_peers_path(&peers);
        Self {
            app,
            peers,
            _xdg: xdg,
            _peers_dir: peers_dir,
        }
    }

    fn open_connect(&mut self) {
        self.app.start_connect();
        assert_eq!(self.app.state.view, View::Connect);
        assert_eq!(
            self.app.state.connect.as_ref().expect("form").mode,
            FormMode::Key
        );
    }

    fn fill_connect(&mut self, address: &str, port: u16, key: &str) {
        let form = self.app.state.connect.as_mut().expect("connect form");
        form.address = address.to_string();
        form.port = port.to_string();
        form.key = key.to_string();
    }

    fn fill_mcp(&mut self, address: &str, port: u16, key: &str) {
        self.app.start_mcp_pair();
        let form = self.app.state.mcp_pair.as_mut().expect("MCP form");
        form.address = address.to_string();
        form.port = port.to_string();
        form.key = key.to_string();
    }

    fn form_error(&self) -> String {
        let form = self.app.state.connect.as_ref().expect("form stays open");
        form.error.clone().expect("an error is shown")
    }

    fn mcp_error(&self) -> String {
        self.app
            .state
            .mcp_pair
            .as_ref()
            .and_then(|form| form.error.clone())
            .expect("the MCP form shows the reason")
    }

    /// The open Connect form, after a failure, for a second attempt.
    fn edit_connect(&mut self, edit: impl FnOnce(&mut serial_nexus_terminal::state::ConnectForm)) {
        let form = self.app.state.connect.as_mut().expect("connect form");
        edit(form);
    }
}

fn human_key(info: &ShareSessionInfo) -> String {
    SharingKey::encode(&info.host, &info.human).expect("human key")
}

fn ai_key(info: &ShareSessionInfo) -> String {
    SharingKey::encode(&info.host, &info.ai).expect("AI key")
}

fn address_of(info: &ShareSessionInfo) -> String {
    info.host.address.ip().to_string()
}

fn assert_no_secret_in(text: &str, info: &ShareSessionInfo) {
    assert!(!text.contains(&info.human.credential), "{text}");
    assert!(!text.contains(&info.ai.credential), "{text}");
    assert!(!text.contains(&human_key(info)), "{text}");
    assert!(!text.contains(&ai_key(info)), "{text}");
}

#[test]
fn a_human_key_pairs_and_wrong_role_host_or_format_is_refused_without_dialling() {
    let _guard = serial_test();
    let device = SimulatedDevice::new();
    let host = start_host(&device);
    let (_local, socket) = start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let mut terminal = Terminal::new(&socket);

    // An AI key is refused where a human key belongs, before any connection.
    terminal.open_connect();
    terminal.fill_connect(
        &address_of(&host.info),
        host.info.host.address.port(),
        &ai_key(&host.info),
    );
    terminal.app.submit_connect();
    let error = terminal.form_error();
    assert!(error.contains("MCP/AI"), "{error}");
    assert_no_secret_in(&error, &host.info);
    assert!(terminal.app.state.remote_labels.is_empty());
    assert!(!terminal
        .app
        .state
        .devices
        .iter()
        .any(|entry| entry.endpoint != serial_nexus_terminal::state::LOCAL_ENDPOINT));

    // A key for another host is refused with both addresses, not dialled.
    terminal.edit_connect(|form| {
        form.address = "127.0.0.2".into();
        form.key = human_key(&host.info);
    });
    terminal.app.submit_connect();
    let error = terminal.form_error();
    assert!(error.contains("127.0.0.1"), "{error}");
    assert!(error.contains("127.0.0.2"), "{error}");
    assert_no_secret_in(&error, &host.info);

    // Malformed and oversized pastes fail clearly and never print the value.
    for bad in [
        "not-a-key".to_string(),
        format!("snx1.human.{}", "A".repeat(8_000)),
        format!("{}.", human_key(&host.info)),
    ] {
        terminal.edit_connect(|form| form.key = bad.clone());
        terminal.app.submit_connect();
        let error = terminal.form_error();
        assert!(!error.is_empty());
        assert!(!error.contains(&bad), "{error}");
        assert_no_secret_in(&error, &host.info);
    }

    // A paste is validated as pasted: an inner space or an oversized value is
    // refused whole, never silently repaired into an accepted key.
    terminal.open_connect();
    terminal.fill_connect(&address_of(&host.info), host.info.host.address.port(), "");
    terminal.app.state.connect.as_mut().expect("form").field =
        serial_nexus_terminal::state::ConnectField::Key;
    let human = human_key(&host.info);
    terminal
        .app
        .handle_paste(&format!("{} {}", &human[..40], &human[40..]));
    let form = terminal.app.state.connect.as_ref().expect("form");
    assert!(form.key.is_empty(), "nothing was inserted");
    assert!(
        form.error.as_deref().unwrap_or("").contains("paste"),
        "{:?}",
        form.error
    );
    terminal
        .app
        .handle_paste(&format!("snx1.human.{}", "A".repeat(5_000)));
    let form = terminal.app.state.connect.as_ref().expect("form");
    assert!(form.key.is_empty(), "nothing was inserted");
    assert!(
        form.error.as_deref().unwrap_or("").contains("too long"),
        "{:?}",
        form.error
    );
    assert!(
        terminal.app.state.remote_labels.is_empty(),
        "nothing dialled"
    );

    // The right key, address and port connect and show the host's devices.
    terminal.edit_connect(|form| {
        form.address = address_of(&host.info);
        form.key = human_key(&host.info);
    });
    terminal.app.submit_connect();
    assert!(
        terminal.app.state.connect.is_none(),
        "a successful pair closes the form"
    );
    assert_eq!(terminal.app.state.remote_labels.len(), 1);
    assert!(terminal.app.state.devices.iter().any(|entry| entry.endpoint
        != serial_nexus_terminal::state::LOCAL_ENDPOINT
        && entry.path == device.path));
    terminal.app.shutdown();
}

#[test]
fn a_rotated_session_makes_the_previous_key_fail_without_leaking_it() {
    let _guard = serial_test();
    let device = SimulatedDevice::new();
    let host = start_host(&device);
    let old = host.info.clone();
    let (_local, socket) = start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let mut terminal = Terminal::new(&socket);

    terminal.open_connect();
    terminal.fill_connect(&address_of(&old), old.host.address.port(), &human_key(&old));
    terminal.app.submit_connect();
    assert!(terminal.app.state.connect.is_none(), "the first key pairs");

    // Stop sharing and start again on the same address: fresh CA and secrets.
    host.owner.stop_sharing().expect("stop sharing");
    let rotated = host
        .owner
        .start_sharing(
            ShareBind::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            vec![device.path.clone()],
        )
        .expect("sharing restarts");
    assert_ne!(rotated.session_id, old.session_id);
    assert_ne!(rotated.human.credential, old.human.credential);

    terminal.open_connect();
    let port = rotated.host.address.port();
    terminal.fill_connect(&address_of(&rotated), port, &human_key(&old));
    // The old key names the old session's certificate.
    terminal.app.submit_connect();
    let error = terminal.form_error();
    assert!(error.contains("not connected"), "{error}");
    assert_no_secret_in(&error, &rotated);
    assert_no_secret_in(&error, &old);

    terminal.edit_connect(|form| form.key = human_key(&rotated));
    terminal.app.submit_connect();
    assert!(
        terminal.app.state.connect.is_none(),
        "the new session's key works"
    );
    terminal.app.shutdown();
}

#[test]
fn an_older_host_still_connects_through_the_advanced_fields() {
    let _guard = serial_test();
    let device = SimulatedDevice::new();
    let host = start_host(&device);
    let (_local, socket) = start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let mut terminal = Terminal::new(&socket);

    terminal.open_connect();
    terminal.app.connect_toggle_mode();
    let form = terminal.app.state.connect.as_ref().expect("form");
    assert_eq!(form.mode, FormMode::Advanced);
    let form = terminal.app.state.connect.as_mut().expect("form");
    form.address = address_of(&host.info);
    form.port = host.info.host.address.port().to_string();
    form.ca = host.info.host.ca_cert_der_b64.clone();
    form.fingerprint = host.info.host.host_cert_fingerprint_sha256.clone();
    form.credential = host.info.human.credential.clone();
    terminal.app.submit_connect();

    assert!(
        terminal.app.state.connect.is_none(),
        "the older fields still pair"
    );
    assert_eq!(terminal.app.state.remote_labels.len(), 1);
    terminal.app.shutdown();
}

#[test]
fn the_mcp_pairing_form_adds_replaces_and_preserves_entries_and_never_connects() {
    let _guard = serial_test();
    let device = SimulatedDevice::new();
    let host = start_host(&device);
    let (_local, socket) = start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let mut terminal = Terminal::new(&socket);
    let peers = terminal.peers.clone();

    // A human key is refused where the MCP/AI key belongs; nothing is written.
    terminal.fill_mcp(
        &address_of(&host.info),
        host.info.host.address.port(),
        &human_key(&host.info),
    );
    terminal.app.submit_mcp_pair();
    let error = terminal.mcp_error();
    assert!(error.contains("human"), "{error}");
    assert_no_secret_in(&error, &host.info);
    assert!(!peers.exists(), "a wrong role writes nothing");

    // A valid AI key saves to the owner-only file, without connecting or
    // starting sharing.
    terminal.app.state.mcp_pair.as_mut().expect("form").key = ai_key(&host.info);
    terminal.app.submit_mcp_pair();
    assert!(
        terminal.app.state.mcp_pair.is_none(),
        "a saved pairing closes the form"
    );
    assert!(!terminal.app.state.sharing.active, "sharing stays off");
    assert!(
        terminal.app.state.remote_labels.is_empty(),
        "nothing dialled"
    );
    assert_eq!(
        std::fs::metadata(&peers)
            .expect("file")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let loaded = serial_nexus_client::Peers::load(&peers).expect("load");
    assert_eq!(loaded.len(), 1);
    let endpoint = serial_nexus_client::RemoteEndpoint::from_socket_addr(host.info.host.address);
    assert_eq!(
        loaded.find(&endpoint).expect("entry").secret.role,
        PairingRole::Ai
    );

    // Saving the same endpoint needs one confirmation; Esc keeps the file.
    let before = std::fs::read(&peers).expect("read");
    terminal.fill_mcp(
        &address_of(&host.info),
        host.info.host.address.port(),
        &ai_key(&host.info),
    );
    terminal.app.submit_mcp_pair();
    let error = terminal.mcp_error();
    assert!(error.contains("already paired"), "{error}");
    assert_no_secret_in(&error, &host.info);
    assert_eq!(std::fs::read(&peers).expect("read"), before);
    terminal.app.handle_key(Key::Esc);
    assert!(terminal.app.state.mcp_pair.is_none(), "Esc leaves the form");
    assert_eq!(std::fs::read(&peers).expect("read"), before);

    // Confirming replaces the entry and keeps every other endpoint.
    let other_dir = TempDir::new().expect("other");
    let other_path = other_dir.path().join("mcp-peers.json");
    let other_endpoint =
        serial_nexus_client::RemoteEndpoint::new("192.0.2.44".parse().unwrap(), 7000);
    serial_nexus_client::save_peer(
        &other_path,
        other_endpoint,
        serial_nexus_protocol::PairedHost {
            address: "192.0.2.44:7000".parse().unwrap(),
            host_name: "192.0.2.44".into(),
            ca_cert_der_b64: "Y2E=".into(),
            host_cert_fingerprint_sha256: "ab".repeat(32),
        },
        serial_nexus_protocol::PairingSecret {
            role: PairingRole::Ai,
            credential: "b3RoZXItY3JlZGVudGlhbA".into(),
        },
        false,
    )
    .expect("the other entry saves");
    serial_nexus_client::save_peer(
        &other_path,
        endpoint,
        host.info.host.clone(),
        host.info.ai.clone(),
        false,
    )
    .expect("the host entry is seeded");
    std::fs::copy(&other_path, &peers).expect("seed both entries");
    std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o600)).expect("chmod");

    terminal.fill_mcp(
        &address_of(&host.info),
        host.info.host.address.port(),
        &ai_key(&host.info),
    );
    terminal.app.submit_mcp_pair();
    assert!(terminal.app.state.mcp_pair.is_some(), "first Enter asks");
    assert!(
        terminal
            .app
            .state
            .mcp_pair
            .as_ref()
            .expect("form")
            .confirm_replace
    );
    terminal.app.submit_mcp_pair();
    assert!(
        terminal.app.state.mcp_pair.is_none(),
        "the second confirmation replaces"
    );
    let loaded = serial_nexus_client::Peers::load(&peers).expect("load");
    assert_eq!(loaded.len(), 2, "the other endpoint is preserved");
    assert!(loaded.find(&other_endpoint).is_ok());
    assert!(loaded.find(&endpoint).is_ok());
    terminal.app.shutdown();
}

#[test]
fn a_failed_mcp_save_reports_clearly_and_changes_nothing() {
    let _guard = serial_test();
    let device = SimulatedDevice::new();
    let host = start_host(&device);
    let (_local, socket) = start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let mut terminal = Terminal::new(&socket);

    // An unwritable directory leaves the (absent) file alone and reports a
    // secret-free reason.
    let locked = TempDir::new().expect("dir");
    std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o500)).expect("lock");
    terminal
        .app
        .set_peers_path(locked.path().join("mcp-peers.json"));
    terminal.fill_mcp(
        &address_of(&host.info),
        host.info.host.address.port(),
        &ai_key(&host.info),
    );
    terminal.app.submit_mcp_pair();
    let error = terminal.mcp_error();
    assert!(!error.is_empty());
    assert_no_secret_in(&error, &host.info);
    assert!(!locked.path().join("mcp-peers.json").exists());
    std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o700))
        .expect("unlock");

    // Cancelling keeps an existing file byte-for-byte.
    let peers = terminal.peers.clone();
    terminal.app.set_peers_path(locked.path().join("kept.json"));
    terminal.fill_mcp(
        &address_of(&host.info),
        host.info.host.address.port(),
        &ai_key(&host.info),
    );
    terminal.app.submit_mcp_pair();
    assert!(terminal.app.state.mcp_pair.is_none(), "saved");
    let kept = locked.path().join("kept.json");
    let before = std::fs::read(&kept).expect("read");
    terminal.fill_mcp(
        &address_of(&host.info),
        host.info.host.address.port(),
        &ai_key(&host.info),
    );
    terminal.app.submit_mcp_pair();
    assert!(terminal.app.state.mcp_pair.is_some());
    terminal.app.handle_key(Key::Esc);
    assert_eq!(std::fs::read(&kept).expect("read"), before);
    assert!(!peers.exists(), "the unrelated path was never touched");
    terminal.app.shutdown();
}
