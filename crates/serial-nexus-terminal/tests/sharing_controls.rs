//! The host's sharing controls against a real core: a session can only start
//! deliberately, unchecking revokes remote access at the core, bind failures
//! leave sharing off, and secrets live only in memory.

mod support;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serial_nexus_client::{ClientInfo, RemoteClient};

use serial_nexus_protocol::{PairedHost, PairingRole, PairingSecret, SharingSnapshot};
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::settings::SettingsStore;
use serial_nexus_terminal::state::{
    Checklist, Notice, Severity, ShareConfirm, ShareRow, View, SETTINGS_FIXED_ROWS,
};
use support::{connect, start_core, PortListerGuard, SimulatedDevice};
use tempfile::TempDir;

/// The port lister is process-wide, so sharing tests take turns.
static SHARING_TESTS: Mutex<()> = Mutex::new(());

fn serial_test() -> MutexGuard<'static, ()> {
    // A failed earlier test poisons the lock; the seam is process-wide but
    // each test clears and reinstalls it, so the next one may still run.
    SHARING_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct Fixture {
    _core: TempDir,
    _xdg: TempDir,
    socket: PathBuf,
    settings_path: PathBuf,
    devices: Arc<Mutex<Vec<PathBuf>>>,
    _lister: PortListerGuard,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> (Self, Vec<SimulatedDevice>) {
        let guard = serial_test();
        let (core, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
        let simulated = vec![SimulatedDevice::new(), SimulatedDevice::new()];
        let paths: Vec<PathBuf> = simulated
            .iter()
            .map(|device| PathBuf::from(&device.path))
            .collect();
        let devices = Arc::new(Mutex::new(paths));
        let lister = PortListerGuard::from_paths(Arc::clone(&devices));
        let xdg = TempDir::new().expect("xdg");
        let settings_path = xdg.path().join("serial-nexus/terminal.json");
        (
            Self {
                _core: core,
                _xdg: xdg,
                socket,
                settings_path,
                devices,
                _lister: lister,
                _guard: guard,
            },
            simulated,
        )
    }

    fn app(&self) -> App {
        let settings = SettingsStore::at(&self.settings_path)
            .load_or_create()
            .expect("settings");
        let client = connect(&self.socket, ClientInfo::human("sharing-terminal"));
        App::with_preferences(
            client,
            40,
            100,
            settings,
            Some(SettingsStore::at(&self.settings_path)),
        )
    }

    fn app_without_store(&self) -> App {
        let settings = SettingsStore::at(&self.settings_path)
            .load_or_create()
            .expect("settings");
        let client = connect(&self.socket, ClientInfo::human("sharing-terminal"));
        App::with_settings(client, 40, 100, settings)
    }

    fn replace_devices(&self, paths: Vec<PathBuf>) {
        *self.devices.lock().expect("devices lock") = paths;
    }

    fn settings_text(&self) -> String {
        std::fs::read_to_string(&self.settings_path).unwrap_or_default()
    }
}

fn check_device(app: &mut App, path: &str) {
    app.state.begin_settings();
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("device is detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + index;
    assert_eq!(app.state.toggle_draft(), Some(true));
}

fn set_bind(app: &mut App, address: &str, port: u16) {
    app.state.draft.share_address = address.into();
    app.state.draft.share_port = port;
    app.state.port_text = port.to_string();
}

fn save(app: &mut App) {
    assert_eq!(app.state.view, serial_nexus_terminal::state::View::Settings);
    app.handle_key(serial_nexus_terminal::input::Key::Ctrl('s'));
}

fn start(app: &mut App) {
    // Starting is an explicit action now that `s` only opens the Share page.
    // The page's own mouse/keyboard flow is covered by the PTY tests.
    app.start_sharing();
}

fn pairing(app: &App) -> (PairedHost, PairingSecret, PairingSecret) {
    let info = app
        .state
        .sharing
        .pairing
        .as_ref()
        .expect("a running session shows pairing information");
    (info.host.clone(), info.human.clone(), info.ai.clone())
}

#[test]
fn saving_checks_starts_a_session_and_shows_both_credentials_and_host_authentication() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();

    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    assert_eq!(
        app.state.view,
        serial_nexus_terminal::state::View::Stream,
        "Save returns to the stream"
    );
    assert!(app.state.sharing.pending_checked.contains(&path));
    assert!(!app.state.sharing.active);

    start(&mut app);
    assert!(app.state.sharing.active, "sharing started");
    assert_eq!(app.state.view, serial_nexus_terminal::state::View::Share);
    let (host, human, ai) = pairing(&app);
    assert_eq!(human.role, PairingRole::Human);
    assert_eq!(ai.role, PairingRole::Ai);
    assert_ne!(human.credential, ai.credential);
    assert!(!host.ca_cert_der_b64.is_empty());
    assert_eq!(host.host_cert_fingerprint_sha256.len(), 64);
    assert!(
        app.state
            .sharing
            .snapshot
            .as_ref()
            .expect("snapshot")
            .checked
            .iter()
            .any(|device| device.path == path),
        "the core confirms the checked device"
    );

    // The secrets are never written to the settings file.
    let saved = fixture.settings_text();
    assert!(!saved.contains(&human.credential));
    assert!(!saved.contains(&ai.credential));
    assert!(!saved.contains("credential"));

    // Debug never prints a credential value.
    let debug = format!("{:?}", human);
    assert!(
        !debug.contains(&human.credential),
        "Debug prints no credential"
    );

    app.shutdown();
}

#[test]
fn the_checked_device_can_be_opened_by_a_remote_client_though_the_host_never_opened_it() {
    let (fixture, mut devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();

    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    let (host, human, _ai) = pairing(&app);

    let remote = RemoteClient::connect(
        &host,
        &human,
        Some("remote-human".into()),
        Duration::from_secs(5),
    )
    .expect("the paired human client authenticates");
    assert_eq!(remote.role(), PairingRole::Human);
    let listed = remote.list_devices().expect("remote list");
    assert_eq!(listed.devices.len(), 1, "only the checked device appears");
    assert_eq!(listed.devices[0].path, path);
    assert!(!listed.devices[0].open);

    // An allowed but unopened port opens at the chosen baud rate.
    let opened = remote
        .open(&path, Some(9_600), false)
        .expect("the remote client may open a checked device");
    assert_eq!(opened.baud, 9_600);
    remote
        .write_bytes(opened.device_id, b"remote\r", None)
        .expect("write reaches the core");
    devices[0].read_until(b"remote", Duration::from_secs(5));
    devices[0].write(b"answer\r\n");
    let read = remote.read(opened.device_id, Some(10)).expect("read");
    assert!(
        read.lines.iter().any(|line| line.text.contains("answer")),
        "{:?}",
        read.lines
    );

    app.shutdown();
}

#[test]
fn unchecking_while_sharing_revokes_the_remote_client_and_leaves_other_devices_alone() {
    let (fixture, mut devices) = Fixture::new();
    let mut app = fixture.app();
    let first = devices[0].path.clone();
    let second = devices[1].path.clone();

    app.state.begin_settings();
    for path in [&first, &second] {
        let index = app
            .state
            .devices
            .iter()
            .position(|device| &device.path == path)
            .expect("detected");
        app.state.settings_row = SETTINGS_FIXED_ROWS + index;
        assert_eq!(app.state.toggle_draft(), Some(true));
    }
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    let (host, human, _ai) = pairing(&app);
    let remote = RemoteClient::connect(&host, &human, None, Duration::from_secs(5))
        .expect("remote connects");
    let first_opened = remote.open(&first, Some(9_600), false).expect("open first");
    let second_opened = remote
        .open(&second, Some(9_600), false)
        .expect("open second");
    let local = fixture.app();
    let local_first = local
        .client()
        .open(&first, None, false)
        .expect("a local client joins the same device");

    // Save a checklist that drops the first device.
    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == first)
        .expect("detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + row;
    assert_eq!(app.state.toggle_draft(), Some(false));
    save(&mut app);
    assert!(
        app.state
            .notice
            .as_ref()
            .is_none_or(|notice| !notice.text.contains("not saved")),
        "{:?}",
        app.state.notice
    );
    // The remote session is detached from that device at the core: further
    // writes fail visibly and it disappears from the remote list.
    assert!(
        remote
            .list_devices()
            .unwrap()
            .devices
            .iter()
            .all(|device| device.path != first),
        "the unchecked device is gone from the remote list"
    );
    assert!(
        remote
            .write_bytes(first_opened.device_id, b"x", None)
            .is_err(),
        "a revoked device refuses further writes"
    );
    // The other checked device and the local client are untouched.
    assert!(remote
        .list_devices()
        .unwrap()
        .devices
        .iter()
        .any(|device| device.path == second));
    local
        .client()
        .write_bytes(local_first.device_id, b"local\r", None)
        .expect("the local client keeps its device");
    devices[0].read_until(b"local", Duration::from_secs(5));
    remote
        .write_bytes(second_opened.device_id, b"second\r", None)
        .expect("the other shared device still writes");
    devices[1].read_until(b"second", Duration::from_secs(5));

    app.shutdown();
}

#[test]
fn a_confirmed_active_endpoint_handover_disconnects_old_remotes_and_rotates_keys() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.open_share();

    let old_keys = pairing(&app);
    let old_remote = RemoteClient::connect(&old_keys.0, &old_keys.1, None, Duration::from_secs(5))
        .expect("the old key authenticates");
    assert_eq!(
        old_remote.list_devices().expect("old list").devices.len(),
        1
    );
    let old_bind = app
        .state
        .sharing
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.bind)
        .expect("old listener bound");
    let new_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a free port")
        .local_addr()
        .expect("address")
        .port();
    app.state.draft.share_address = "127.0.0.2".into();
    app.state.port_text = new_port.to_string();
    let save_row = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::Save)
        .expect("Save button");
    app.state.share_row = save_row;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.share_confirm, Some(ShareConfirm::Rebind));
    assert!(
        old_remote.list_devices().is_ok(),
        "confirmation leaves old peers live"
    );

    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert!(app.state.sharing.active);
    assert_eq!(app.state.settings.share_port, new_port);
    assert_eq!(app.state.settings.share_address, "127.0.0.2");
    assert_eq!(
        app.state
            .sharing
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.bind),
        Some(std::net::SocketAddr::from(([127, 0, 0, 2], new_port)))
    );
    assert_ne!(
        pairing(&app),
        old_keys,
        "new session issues fresh long keys"
    );
    assert!(
        old_remote.list_devices().is_err(),
        "handover disconnects old peers"
    );
    let (new_host, new_human, _) = pairing(&app);
    let new_remote = RemoteClient::connect(&new_host, &new_human, None, Duration::from_secs(5))
        .expect("fresh key authenticates at replacement endpoint");
    assert_eq!(
        new_remote.list_devices().expect("new list").devices.len(),
        1
    );
    assert_ne!(old_bind.port(), new_port);
    assert!(!app.state.has_unsaved_share());
    assert!(!fixture.settings_text().contains(&new_human.credential));
    app.shutdown();
}

#[test]
fn a_failed_settings_publish_does_not_detach_remote_sessions() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();

    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    app.start_sharing();
    let (host, human, _) = pairing(&app);
    let remote = RemoteClient::connect(&host, &human, None, Duration::from_secs(5))
        .expect("remote connects");
    let opened = remote.open(&path, Some(9_600), false).expect("open device");

    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected device");
    app.state.settings_row = SETTINGS_FIXED_ROWS + row;
    assert_eq!(app.state.toggle_draft(), Some(false));
    app.state.draft.local_echo = false;

    std::fs::remove_file(&fixture.settings_path).expect("remove settings file");
    std::fs::create_dir(&fixture.settings_path).expect("force publication failure");
    save(&mut app);

    assert!(
        app.state.settings.local_echo,
        "failed Save does not apply preferences"
    );
    assert!(app.state.sharing.checked().contains(&path));
    assert!(
        remote.status(opened.device_id).is_ok(),
        "failed Save does not detach an existing remote session"
    );
    assert!(app
        .state
        .notice
        .as_ref()
        .is_some_and(|notice| notice.text.contains("not saved")));
    app.shutdown();
}

#[test]
fn a_bind_failure_is_shown_and_sharing_stays_off() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().unwrap().port();

    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", port);
    save(&mut app);
    start(&mut app);

    assert!(!app.state.sharing.active, "sharing remains off");
    assert!(app.state.sharing.pairing.is_none());
    let notice = app.state.notice.as_ref().expect("the failure is shown");
    assert_eq!(notice.severity, Severity::Error);
    assert!(
        notice.text.contains("sharing not started"),
        "{}",
        notice.text
    );
    app.shutdown();
}

#[test]
fn an_ipv6_loopback_session_binds_and_authenticates() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "::1", 0);
    save(&mut app);
    start(&mut app);
    assert!(app.state.sharing.active, "IPv6 sharing started");
    let (host, human, _ai) = pairing(&app);
    assert!(host.address.is_ipv6(), "{}", host.address);
    RemoteClient::connect(&host, &human, None, Duration::from_secs(5))
        .expect("the IPv6 pairing authenticates");
    app.shutdown();
}

#[test]
fn an_unplugged_device_loses_its_checkbox_and_the_host_is_told() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    let second = devices[1].path.clone();
    check_device(&mut app, &path);
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == second)
        .expect("detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + index;
    assert_eq!(app.state.toggle_draft(), Some(true));
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);

    // Unplug the first device: its node disappears and the core expires it.
    let mut devices = devices;
    drop(devices.remove(0));
    fixture.replace_devices(vec![PathBuf::from(&second)]);
    app.refresh_devices();

    assert!(
        !app.state.sharing.pending_checked.contains(&path),
        "the unplugged device is no longer shared"
    );
    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == second)
        .expect("the remaining device");
    assert_eq!(
        app.state.draft_device_checked(SETTINGS_FIXED_ROWS + row),
        Some(true),
        "the other check is kept"
    );
    app.state.cancel_settings();
    app.shutdown();
}

#[test]
fn a_replacement_at_a_checked_path_needs_an_explicit_recheck() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    assert!(app.state.sharing.pending_checked.contains(&path));

    // The core reports the path as no longer authorized (a different node at
    // the same path); the checkbox must not follow the path blindly.
    let snapshot = SharingSnapshot {
        active: true,
        session_id: Some("session".into()),
        bind: app.state.sharing.snapshot.as_ref().and_then(|s| s.bind),
        checked: Vec::new(),
        remote_clients: 0,
        pending_connections: 0,
    };
    let _ = app.state.apply_sharing_snapshot(snapshot);
    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("the path is still detected");
    assert_eq!(
        app.state.draft_device_checked(SETTINGS_FIXED_ROWS + row),
        Some(false),
        "a replacement must be checked again"
    );
    app.state.cancel_settings();
    app.shutdown();
}

#[test]
fn periodic_device_refresh_preserves_the_authoritative_shared_checklist() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.state.notice = None;

    app.refresh_devices();

    assert!(app.state.sharing.pending_checked.contains(&path));
    assert!(app.state.notice.is_none());
    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("device remains detected");
    assert_eq!(
        app.state.draft_device_checked(SETTINGS_FIXED_ROWS + row),
        Some(true)
    );
    save(&mut app);
    assert!(app.state.sharing.pending_checked.contains(&path));
    app.shutdown();
}

#[test]
fn a_failed_settings_save_applies_neither_the_preferences_nor_the_checklist() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    let replacement_path = devices[1].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);

    app.state.begin_settings();
    app.state.draft.local_echo = false;
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("checked device detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + index;
    assert_eq!(app.state.toggle_draft(), Some(false));
    let replacement_index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == replacement_path)
        .expect("replacement device detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + replacement_index;
    assert_eq!(app.state.toggle_draft(), Some(true));
    std::fs::remove_file(&fixture.settings_path).expect("remove settings file");
    std::fs::create_dir(&fixture.settings_path).expect("block the settings file");
    save(&mut app);

    let notice = app.state.notice.as_ref().expect("the failure is shown");
    assert_eq!(notice.severity, Severity::Error);
    assert!(
        notice.text.contains("settings not saved"),
        "{}",
        notice.text
    );
    assert_eq!(app.state.view, serial_nexus_terminal::state::View::Settings);
    assert!(
        app.state.settings.local_echo,
        "the option in effect stays on"
    );
    assert!(
        !app.state.draft.local_echo,
        "the attempted preference stays in the draft"
    );
    assert!(app.state.sharing.active, "sharing remains active");
    assert!(
        app.state.sharing.pending_checked.contains(&path),
        "the prior core checklist is restored"
    );
    assert!(
        !app.state
            .sharing
            .pending_checked
            .contains(&replacement_path),
        "the failed new checklist is rolled back"
    );
    assert!(
        !app.state.draft_checked.contains(&path),
        "the attempted draft is kept"
    );
    assert!(app.state.draft_checked.contains(&replacement_path));
    app.shutdown();
}

#[test]
fn rechecking_after_stop_retains_the_current_discovery_identity() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);

    app.state.begin_settings();
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + index;
    assert_eq!(app.state.toggle_draft(), Some(false));
    save(&mut app);
    app.stop_sharing();

    check_device(&mut app, &path);
    save(&mut app);
    app.state.begin_settings();
    save(&mut app);

    assert!(app.state.sharing.pending_checked.contains(&path));
    assert_eq!(
        app.state.notice.as_ref().map(|notice| notice.severity),
        Some(Severity::Info)
    );
    app.shutdown();
}

#[test]
fn a_failed_save_keeps_a_checked_draft_when_discovery_identity_is_lost() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    save(&mut app);
    app.state.begin_settings();
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected device");
    app.state.devices[index].identity = None;
    std::fs::remove_file(&fixture.settings_path).expect("remove settings file");
    std::fs::create_dir(&fixture.settings_path).expect("block the settings file");

    save(&mut app);

    assert!(
        app.state.sharing.pending_checked.contains(&path),
        "the previously saved checklist is not changed by a failed save"
    );
    assert!(
        app.state.draft_checked.contains(&path),
        "a failed save retains the human's checklist draft"
    );
    let detected = app.state.detected_local_identities();
    app.state.reconcile_share_checks(&detected);
    assert!(
        app.state.sharing.pending_checked.contains(&path),
        "a later refresh must preserve an existing path while its identity is temporarily unavailable"
    );
    assert!(
        app.state.draft_checked.contains(&path),
        "a later refresh must not discard the attempted checklist draft"
    );
    assert_eq!(
        app.state.notice.as_ref().map(|notice| notice.severity),
        Some(Severity::Error)
    );
    app.shutdown();
}

#[test]
fn leaving_the_terminal_stops_sharing_and_revokes_the_credentials() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    let (host, human, _ai) = pairing(&app);

    app.shutdown();
    assert!(!app.state.sharing.active);
    assert!(app.state.sharing.pairing.is_none(), "secrets are dropped");

    // The listener and the credentials are gone with the owning terminal.
    let result = RemoteClient::connect(&host, &human, None, Duration::from_secs(2));
    assert!(result.is_err(), "the old pairing no longer works");
}

#[test]
fn saving_preferences_still_works_after_adding_a_device_to_active_sharing() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let first = devices[0].path.clone();
    let added = devices[1].path.clone();

    check_device(&mut app, &first);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    app.start_sharing();
    assert!(app.state.sharing.active);

    check_device(&mut app, &added);
    save(&mut app);
    assert_eq!(
        app.state.sharing.pending_checked.identities().get(&added),
        app.state
            .devices
            .iter()
            .find(|device| device.path == added)
            .and_then(|device| device.identity.as_ref()),
        "notice={:?}; pending={:?}; snapshot={:?}",
        app.state.notice,
        app.state.sharing.pending_checked,
        app.state.sharing.snapshot
    );

    app.refresh_devices();
    app.state.begin_settings();
    app.state.draft.local_echo = false;
    save(&mut app);

    assert!(!app.state.settings.local_echo);
    assert!(app.state.sharing.checked().contains(&first));
    assert!(app.state.sharing.checked().contains(&added));
    assert_eq!(
        app.state.notice.as_ref().map(|notice| notice.severity),
        Some(Severity::Info)
    );
    app.shutdown();
}

#[test]
fn saving_preferences_does_not_revoke_a_shared_device_during_temporary_identity_loss() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    app.start_sharing();
    assert!(app.state.sharing.active);
    assert!(app.state.sharing.checked().contains(&path));

    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected device");
    app.state.devices[index].identity = None;
    let snapshot = app
        .state
        .sharing
        .snapshot
        .clone()
        .expect("active sharing snapshot");
    app.state.apply_sharing_snapshot(snapshot.clone());
    app.state.devices[index].identity = Some("snx-refreshed-after-gap".into());
    app.state.apply_sharing_snapshot(snapshot);
    assert!(
        app.state.sharing.pending_checked.contains(&path),
        "the core-authoritative selection survives a refreshed discovery token"
    );

    app.state.begin_settings();
    app.state.draft.local_echo = false;
    save(&mut app);

    assert!(app.state.sharing.active);
    assert!(app.state.sharing.checked().contains(&path));
    assert!(app.state.sharing.pending_checked.contains(&path));
    assert!(app.state.settings.local_echo);
    assert!(
        app.state.notice.as_ref().is_some_and(|notice| {
            notice.severity == Severity::Error && notice.text.contains("cannot verify")
        }),
        "save must refuse rather than silently revoke the checked device"
    );
    app.shutdown();
}

#[test]
fn a_reappeared_path_with_a_new_identity_is_not_shared_without_rechecking() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    app.start_sharing();
    assert!(app.state.sharing.active);
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected device");
    app.state.devices[index].identity = None;
    let snapshot = app
        .state
        .sharing
        .snapshot
        .clone()
        .expect("active sharing snapshot");
    app.state.apply_sharing_snapshot(snapshot);

    app.stop_sharing();
    app.state.devices[index].identity = Some("replacement-generation".into());
    app.start_sharing();

    assert!(!app.state.sharing.active);
    assert!(!app.state.sharing.pending_checked.contains(&path));
    assert!(app.state.notice.as_ref().is_some_and(|notice| {
        notice.severity == Severity::Error && notice.text.contains("check at least one device")
    }));
    app.shutdown();
}

#[test]
fn an_active_selection_without_identity_cannot_adopt_a_replacement_after_stop() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    app.start_sharing();
    assert!(app.state.sharing.active);

    app.state.sharing.pending_checked = [path.clone()].into_iter().collect::<Checklist>();
    app.state.draft_checked = app.state.sharing.pending_checked.clone();
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected device");
    app.state.devices[index].identity = None;
    let snapshot = app
        .state
        .sharing
        .snapshot
        .clone()
        .expect("active sharing snapshot");
    app.state.apply_sharing_snapshot(snapshot);

    app.stop_sharing();
    app.state.devices[index].identity = Some("replacement-generation".into());
    app.start_sharing();

    assert!(!app.state.sharing.active);
    assert!(!app.state.sharing.pending_checked.contains(&path));
    app.shutdown();
}

#[test]
fn sharing_does_not_start_when_a_checked_device_has_no_current_identity() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected device");
    app.state.devices[index].identity = None;
    assert!(!app.state.sharing.active, "test starts with sharing off");

    app.start_sharing();

    assert!(!app.state.sharing.active);
    assert!(!app.state.sharing.pending_checked.contains(&path));
    assert!(
        app.state
            .notice
            .as_ref()
            .is_some_and(|notice| notice.text.contains("check at least one device")),
        "the path without an identity is refused"
    );
    app.shutdown();
}

#[test]
fn starting_without_a_checked_device_refuses_and_stays_off() {
    let (fixture, _devices) = Fixture::new();
    let mut app = fixture.app();
    set_bind(&mut app, "127.0.0.1", 0);
    start(&mut app);
    assert!(!app.state.sharing.active);
    let notice = app.state.notice.as_ref().expect("a notice");
    assert!(
        notice.text.contains("check at least one device"),
        "{}",
        notice.text
    );
    app.shutdown();
}

#[test]
fn the_checklist_is_session_only_and_never_written_to_the_settings_file() {
    let (fixture, devices) = Fixture::new();
    let path = devices[0].path.clone();
    let mut app = fixture.app();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.shutdown();

    let saved = fixture.settings_text();
    assert!(
        !saved.contains(&path),
        "the checklist is not persisted: {saved}"
    );
    // A fresh launch defaults to nothing checked and sharing off.
    let app = fixture.app();
    assert!(!app.state.sharing.active);
    assert!(app.state.sharing.pending_checked.is_empty());
    let mut app = app;
    app.state.begin_settings();
    assert_eq!(app.state.draft_checked, BTreeSet::new());
    app.shutdown();
}

#[test]
fn the_share_page_opens_without_a_session_and_starts_nothing() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);

    if app.state.focus != serial_nexus_terminal::state::Focus::Sidebar {
        app.handle_key(serial_nexus_terminal::input::Key::Ctrl(']'));
    }
    app.handle_key(serial_nexus_terminal::input::Key::Char('p'));
    assert_eq!(app.state.view, serial_nexus_terminal::state::View::Share);
    assert!(
        !app.state.sharing.active,
        "opening the Share page never starts listening"
    );
    assert!(
        app.state.sharing.pairing.is_none(),
        "no credentials exist before an explicit Start"
    );
    app.shutdown();
}

#[test]
fn secrets_are_not_put_in_notices() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    let (_, human, ai) = pairing(&app);
    let notices: Vec<String> = app
        .state
        .notice
        .as_ref()
        .map(|notice| vec![notice.text.clone()])
        .unwrap_or_default();
    for text in notices {
        assert!(!text.contains(&human.credential));
        assert!(!text.contains(&ai.credential));
    }
    let notice: Option<Notice> = app.state.notice.clone();
    assert!(notice.is_some());
    app.shutdown();
}

#[test]
fn the_sidebar_share_entry_only_opens_the_page_and_a_dirty_start_needs_confirmation() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);

    if app.state.focus != serial_nexus_terminal::state::Focus::Sidebar {
        app.handle_key(serial_nexus_terminal::input::Key::Ctrl(']'));
    }
    app.handle_key(serial_nexus_terminal::input::Key::Char('s'));
    assert_eq!(app.state.view, View::Share);
    assert!(!app.state.sharing.active, "opening the page starts nothing");

    // A dirty address cannot start on the first activation.
    app.state.draft.share_address = "127.0.0.2".into();
    let start = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::StartStop)
        .expect("a Start button");
    app.state.share_row = start;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.share_confirm, Some(ShareConfirm::Start));
    assert!(!app.state.sharing.active, "the first press only asks");

    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert!(
        app.state.sharing.active,
        "Save and start saves, then starts"
    );
    assert_eq!(app.state.settings.share_address, "127.0.0.2");
    app.shutdown();
}

#[test]
fn a_failed_dirty_start_keeps_the_draft_and_leaves_sharing_off() {
    let (fixture, _devices) = Fixture::new();
    let mut app = fixture.app_without_store();
    app.state.begin_share();
    app.state.draft.share_address = "127.0.0.2".into();
    let start = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::StartStop)
        .expect("a Start button");
    app.state.share_row = start;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    app.handle_key(serial_nexus_terminal::input::Key::Enter);

    assert!(!app.state.sharing.active);
    assert_eq!(
        app.state.draft.share_address, "127.0.0.2",
        "the human's draft is kept for another try"
    );
    assert!(
        app.state
            .share_error
            .as_deref()
            .is_some_and(|error| error.contains("no settings file")),
        "the reason is in the form: {:?}",
        app.state.share_error
    );
    app.shutdown();
}

#[test]
fn stopping_from_the_share_page_needs_an_explicit_second_confirmation() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    assert!(app.state.sharing.active);

    app.open_share();
    let stop = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::StartStop)
        .expect("a Stop button");
    app.state.share_row = stop;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.share_confirm, Some(ShareConfirm::Stop));
    assert!(app.state.sharing.active, "the first press only asks");

    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert!(!app.state.sharing.active, "the second press stops it");
    app.shutdown();
}

#[test]
fn active_share_edits_apply_only_on_save_and_cancel_keeps_the_effective_values() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.open_share();
    let effective = app
        .state
        .sharing
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.bind)
        .expect("the original listener is bound");

    app.state.port_text = "7444".into();
    assert!(app.state.has_unsaved_share());
    let cancel = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::Cancel)
        .expect("a Cancel button");
    app.state.share_row = cancel;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.settings.share_port, 0);
    assert!(!app.state.has_unsaved_share());

    let new_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a new endpoint")
        .local_addr()
        .expect("address")
        .port();
    app.state.port_text = new_port.to_string();
    let save_row = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::Save)
        .expect("a Save button");
    app.state.share_row = save_row;
    let old_keys = pairing(&app);
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.share_confirm, Some(ShareConfirm::Rebind));
    assert_eq!(
        app.state
            .sharing
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.bind),
        Some(effective),
        "the first Save asks and leaves the old session untouched"
    );
    assert_eq!(pairing(&app), old_keys);
    app.handle_key(serial_nexus_terminal::input::Key::Esc);
    assert_eq!(app.state.share_confirm, None);
    assert_eq!(app.state.settings.share_port, 0);
    assert_eq!(pairing(&app), old_keys);
    assert!(
        app.state.has_unsaved_share(),
        "Cancel keeps the editable draft"
    );

    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.share_confirm, Some(ShareConfirm::Rebind));
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert!(app.state.sharing.active);
    assert_eq!(app.state.settings.share_port, new_port);
    assert!(!app.state.has_unsaved_share());
    assert_eq!(
        app.state
            .sharing
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.bind)
            .map(|bind| bind.port()),
        Some(new_port),
        "confirmed Save activates the new listener"
    );
    assert_ne!(pairing(&app), old_keys, "handover rotates long keys");
    app.shutdown();
}

#[test]
fn saving_a_dirty_share_keeps_the_start_button_selected() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    app.open_share();
    app.state.port_text = "7444".into();
    let save_row = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::Save)
        .expect("Save is shown for a dirty draft");
    app.state.share_row = save_row;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert!(!app.state.sharing.active, "Save alone starts nothing");
    assert!(
        app.state.share_row < app.state.share_rows().len(),
        "the selection stays on a row that exists"
    );
    assert_eq!(
        app.state.selected_share_row(),
        ShareRow::StartStop,
        "the next Enter lands on the visible, highlighted Start button"
    );
    app.shutdown();
}

#[test]
fn a_failed_active_endpoint_change_keeps_the_previous_listener_and_draft() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.open_share();
    let old_bind = app
        .state
        .sharing
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.bind)
        .expect("the old listener is bound");
    let old_keys = pairing(&app);
    let remote = RemoteClient::connect(&old_keys.0, &old_keys.1, None, Duration::from_secs(5))
        .expect("remote authenticates before handover");
    let remote_device = remote
        .open(&path, Some(9_600), false)
        .expect("remote opens checked device");

    // The destination address/port is already occupied by another listener.
    // Candidate preparation must fail without dropping the old session.
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let occupied_port = occupied.local_addr().expect("addr").port();
    app.state.port_text = occupied_port.to_string();
    let save_row = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::Save)
        .expect("Save is shown");
    app.state.share_row = save_row;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.share_confirm, Some(ShareConfirm::Rebind));
    app.handle_key(serial_nexus_terminal::input::Key::Enter);

    assert!(
        app.state.sharing.active,
        "the old listener stays up when the new endpoint cannot bind"
    );
    assert_eq!(
        app.state
            .sharing
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.bind),
        Some(old_bind),
        "the effective listener did not move"
    );
    assert_eq!(
        app.state.settings.share_port, 0,
        "preferences did not change"
    );
    assert_eq!(app.state.port_text, occupied_port.to_string());
    assert_eq!(
        pairing(&app),
        old_keys,
        "failed bind preserves the old keys"
    );
    assert!(
        remote.status(remote_device.device_id).is_ok(),
        "failed candidate binding preserves existing remote clients"
    );
    assert!(
        app.state.has_unsaved_share(),
        "the draft is kept for correction"
    );
    assert!(
        app.state
            .share_error
            .as_deref()
            .is_some_and(|error| error.contains("could not bind")),
        "{:?}",
        app.state.share_error
    );
    app.shutdown();
}

#[test]
fn a_failed_active_rebind_settings_publish_keeps_old_session_keys_and_remotes() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.open_share();
    let old_keys = pairing(&app);
    let remote = RemoteClient::connect(&old_keys.0, &old_keys.1, None, Duration::from_secs(5))
        .expect("remote authenticates");
    let old_bind = app
        .state
        .sharing
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.bind)
        .expect("old listener bound");
    let new_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a free port")
        .local_addr()
        .expect("address")
        .port();
    app.state.port_text = new_port.to_string();
    std::fs::remove_file(&fixture.settings_path).expect("remove settings file");
    std::fs::create_dir(&fixture.settings_path).expect("make publish fail after staging");
    let save_row = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::Save)
        .expect("Save");
    app.state.share_row = save_row;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.share_confirm, Some(ShareConfirm::Rebind));
    app.handle_key(serial_nexus_terminal::input::Key::Enter);

    assert!(app.state.sharing.active);
    assert_eq!(
        app.state
            .sharing
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.bind),
        Some(old_bind)
    );
    assert_eq!(pairing(&app), old_keys);
    assert!(
        remote.list_devices().is_ok(),
        "old clients survive a persistence failure"
    );
    assert!(
        app.state.has_unsaved_share(),
        "the failed endpoint draft remains editable"
    );
    assert!(
        app.state
            .share_error
            .as_deref()
            .is_some_and(|message| message.contains("directory")),
        "{:?}",
        app.state.share_error
    );
    let released = std::net::TcpListener::bind(("127.0.0.1", new_port))
        .expect("cancel releases the prepared candidate listener");
    drop(released);
    std::fs::remove_dir(&fixture.settings_path).expect("remove forced failure directory");
    app.shutdown();
}

#[test]
fn an_ipv6_endpoint_spelling_does_not_block_an_active_checklist_save() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let first = devices[0].path.clone();
    let second = devices[1].path.clone();
    check_device(&mut app, &first);
    set_bind(&mut app, "0:0:0:0:0:0:0:1", 0);
    save(&mut app);
    start(&mut app);
    assert!(app.state.sharing.active);
    app.open_share();

    let second_index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == second)
        .expect("the second device");
    let row = app
        .state
        .share_rows()
        .iter()
        .position(|candidate| *candidate == ShareRow::Device(second_index))
        .expect("its checkbox");
    app.state.share_row = row;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);

    let save_row = app
        .state
        .share_rows()
        .iter()
        .position(|candidate| *candidate == ShareRow::Save)
        .expect("Save is shown");
    app.state.share_row = save_row;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);

    assert!(
        app.state.sharing.active,
        "the unchanged endpoint keeps running"
    );
    assert!(
        app.state.share_error.is_none(),
        "the checklist save was not mistaken for an endpoint change: {:?}",
        app.state.share_error
    );
    assert!(
        app.state.sharing.checked().contains(&second),
        "the newly checked device is shared"
    );
    app.shutdown();
}

#[test]
fn an_empty_checklist_cannot_destroy_the_running_share() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.open_share();
    let old_bind = app
        .state
        .sharing
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.bind)
        .expect("the old listener is bound");

    // Uncheck every device and ask to move the endpoint at the same time.
    app.state.draft_checked = Checklist::new();
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("free port");
    let free_port = free.local_addr().expect("addr").port();
    drop(free);
    app.state.port_text = free_port.to_string();
    let save_row = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::Save)
        .expect("Save is shown");
    app.state.share_row = save_row;
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert_eq!(app.state.share_confirm, Some(ShareConfirm::Rebind));
    app.handle_key(serial_nexus_terminal::input::Key::Enter);

    assert!(
        app.state.sharing.active,
        "the running share was not stopped"
    );
    assert_eq!(
        app.state
            .sharing
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.bind),
        Some(old_bind),
        "the listener did not move"
    );
    assert_eq!(
        app.state.settings.share_port, 0,
        "preferences did not change"
    );
    assert!(
        app.state
            .share_error
            .as_deref()
            .is_some_and(|error| error.contains("at least one device")),
        "{:?}",
        app.state.share_error
    );
    app.shutdown();
}

#[test]
fn ctrl_s_on_share_saves_and_never_starts_a_listener() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    app.open_share();
    app.state.share_row = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::StartStop)
        .expect("Start row");
    app.handle_key(serial_nexus_terminal::input::Key::Ctrl('s'));
    assert!(
        !app.state.sharing.active,
        "Ctrl+S saves the draft, it does not activate Start"
    );
    app.state.port_text = "7444".into();
    app.handle_key(serial_nexus_terminal::input::Key::Ctrl('s'));
    assert_eq!(
        app.state.settings.share_port, 7444,
        "Ctrl+S saved the draft"
    );
    assert!(!app.state.sharing.active);
    app.shutdown();
}

#[test]
fn the_mouse_wheel_reveals_compact_advanced_connect_fields() {
    let (fixture, _devices) = Fixture::new();
    let mut app = fixture.app();
    app.start_connect();
    app.connect_toggle_mode();
    if let Some(form) = &mut app.state.connect {
        form.field = serial_nexus_terminal::state::ConnectField::Address;
    }
    let area = ratatui::layout::Rect::new(0, 0, 80, 10);
    app.handle_mouse(
        area,
        crossterm::event::MouseEventKind::ScrollDown,
        45,
        3,
        std::time::Instant::now(),
    );
    assert_ne!(
        app.state.connect.as_ref().expect("form").field,
        serial_nexus_terminal::state::ConnectField::Address,
        "the wheel advances the visible Connect field"
    );
    app.shutdown();
}

#[test]
fn clicking_the_already_selected_connect_field_keeps_its_validation_reason() {
    let (fixture, _devices) = Fixture::new();
    let mut app = fixture.app();
    app.start_connect();
    app.submit_connect();
    assert!(
        app.state
            .connect
            .as_ref()
            .and_then(|form| form.error.as_ref())
            .is_some(),
        "the empty address is reported"
    );
    // The first visible Connect field is the address; clicking it changes
    // nothing and must not erase the reason.
    app.handle_click(ratatui::layout::Rect::new(0, 0, 80, 24), 45, 1);
    assert!(
        app.state
            .connect
            .as_ref()
            .and_then(|form| form.error.as_ref())
            .is_some(),
        "a no-op click keeps actionable validation feedback"
    );
    app.handle_click(ratatui::layout::Rect::new(0, 0, 80, 24), 45, 2);
    assert!(
        app.state
            .connect
            .as_ref()
            .and_then(|form| form.error.as_ref())
            .is_some(),
        "moving between fields is not a correction"
    );
    app.shutdown();
}

#[test]
fn clicking_a_share_error_cannot_activate_the_control_under_it() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().expect("addr").port();
    set_bind(&mut app, "127.0.0.1", port);
    save(&mut app);
    app.open_share();
    app.state.share_row = app
        .state
        .share_rows()
        .iter()
        .position(|row| *row == ShareRow::StartStop)
        .expect("Start");
    app.handle_key(serial_nexus_terminal::input::Key::Enter);
    assert!(!app.state.sharing.active);
    assert!(app.state.share_error.is_some());
    drop(occupied);

    // At 40x7 the error is painted over the last content row. A click there
    // must not resolve to a hidden Start/Stop control.
    app.handle_click(ratatui::layout::Rect::new(0, 0, 40, 7), 2, 4);
    assert!(
        !app.state.sharing.active,
        "clicking an error is not clicking a hidden Start button"
    );
    app.shutdown();
}
