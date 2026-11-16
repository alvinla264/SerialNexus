//! The host's sharing controls in the real terminal: the sidebar button, the
//! settings checklist, the pairing screen with its secrets, and a stopped
//! session when the terminal leaves.

mod support;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use support::tui::{
    TuiSession, KEY_CTRL_BRACKET, KEY_CTRL_S, KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_TAB, KEY_UP,
};
use support::{start_core, PortListerGuard, SimulatedDevice};
use tempfile::TempDir;

const ROWS: u16 = 30;
const COLS: u16 = 110;

static SHARING_TESTS: Mutex<()> = Mutex::new(());

fn serial_test() -> MutexGuard<'static, ()> {
    SHARING_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct Fixture {
    _core: TempDir,
    _xdg: TempDir,
    xdg: PathBuf,
    socket: PathBuf,
    device: SimulatedDevice,
    _lister: PortListerGuard,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let guard = serial_test();
        let (core, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
        let device = SimulatedDevice::new();
        let lister = PortListerGuard::from_path(device.path.clone());
        let xdg = TempDir::new().expect("xdg");
        Self {
            _core: core,
            socket,
            xdg: xdg.path().to_path_buf(),
            _xdg: xdg,
            device,
            _lister: lister,
            _guard: guard,
        }
    }

    fn tui(&self) -> TuiSession {
        TuiSession::spawn(&self.socket, &self.xdg, ROWS, COLS)
    }

    fn settings_text(&self) -> String {
        std::fs::read_to_string(self.xdg.join("serial-nexus/terminal.json")).unwrap_or_default()
    }
}

fn row_with(tui: &TuiSession, needle: &str) -> u16 {
    (0..tui.rows())
        .find(|row| tui.row(*row).contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{}", tui.text()))
}

/// Every complete key the TUI asked the terminal to copy, in order. Never
/// printed by a test; only prefix and length are asserted.
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

fn wait_for_copied_key(tui: &mut TuiSession, prefix: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if copied_keys(&tui.raw_transcript())
            .iter()
            .any(|key| key.starts_with(prefix))
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no complete copy request for {prefix}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn click_text(tui: &mut TuiSession, needle: &str) {
    let row = row_with(tui, needle);
    let column = tui.row(row).find(needle).expect("the needle is in its row") as u16;
    tui.click(column, row);
}

/// Select a Share-page control using its keyboard-visible row, then activate
/// it explicitly with Enter. This exercises the same path as keyboard users.
fn activate_share_button(tui: &mut TuiSession, needle: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut direction = KEY_DOWN;
    let mut previous = String::new();
    let mut reached_edge = false;
    loop {
        let selected = (0..tui.rows())
            .filter_map(|row| {
                let line = tui.row(row);
                line.contains("▸").then(|| line.to_string())
            })
            .next()
            .unwrap_or_default();
        if selected.contains(needle) {
            tui.send(KEY_ENTER);
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "could not select {needle:?} with arrows:\n{}",
            tui.text()
        );
        if !previous.is_empty() && selected == previous && !reached_edge {
            direction = if direction == KEY_DOWN {
                KEY_UP
            } else {
                KEY_DOWN
            };
            reached_edge = true;
        }
        previous = selected;
        tui.send(direction);
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Open the Share page from the sidebar and press its explicit Start button.
/// A dirty draft asks for one more confirmation, which this helper accepts.
fn start_sharing(tui: &mut TuiSession) {
    tui.wait_for_text("[ Start sharing ]");
    click_text(tui, "[ Start sharing ]");
    if tui.text().contains("[ Save and start ]") {
        click_text(tui, "[ Save and start ]");
    }
}

/// Stop through the Share page's explicit two-step confirmation.
fn stop_sharing(tui: &mut TuiSession) {
    click_text(tui, "[ Stop sharing ]");
    tui.wait_for_text("[ Confirm stop ]");
    click_text(tui, "[ Confirm stop ]");
    tui.wait_for_text("sharing stopped");
}

fn open_settings(tui: &mut TuiSession) {
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
}

fn check_device(tui: &mut TuiSession, path: &str) {
    // The sidebar row also names the device; the checkbox is the settings row
    // in the right pane.
    let row = (0..tui.rows())
        .find(|row| {
            let line = tui.row(*row);
            line.contains("[ ]") && line.contains(path)
        })
        .unwrap_or_else(|| panic!("no checkbox for {path}:\n{}", tui.text()));
    let column = tui.row(row).find(path).expect("the path is in its row") as u16;
    tui.click(column, row);
    tui.wait_until("the device checkbox to be checked", |text| {
        text.lines()
            .any(|line| line.contains("[x]") && line.contains(path))
    });
}

fn set_port(tui: &mut TuiSession, port: u16) {
    click_text(tui, "Share port");
    tui.send(&[0x7f, 0x7f, 0x7f, 0x7f]);
    tui.send_text(&port.to_string());
    tui.send(KEY_ENTER);
    tui.wait_until("the typed port", |text| {
        text.lines()
            .find(|line| line.contains("Share port"))
            .is_some_and(|line| line.trim_end().ends_with(&port.to_string()))
    });
}

fn set_port_zero(tui: &mut TuiSession) {
    set_port(tui, 0);
}

#[test]
fn v_shows_a_whole_key_in_an_ordinary_80x24_split_pane() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text(&path);
    open_settings(&mut tui);
    check_device(&mut tui, &path);
    set_port_zero(&mut tui);
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    // Configure at the usual size, then start sharing in a real 80x24
    // terminal, where the stream pane is much narrower than the copy view.
    tui.resize(24, 80);
    std::thread::sleep(Duration::from_millis(300));
    tui.send_text("s");
    tui.wait_until("the Share page at 80x24", |text| {
        text.contains("[ Start sharing ]")
    });
    start_sharing(&mut tui);
    tui.wait_for_text("Human terminal key");
    activate_share_button(&mut tui, "[ Show human key ]");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !tui
        .raw_transcript()
        .windows("snx1.human.".len())
        .any(|window| window == b"snx1.human.")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "a 80x24 terminal shows the whole key: {}",
            tui.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    tui.send_text("x");
    tui.wait_for_text("Human terminal key");
    // Quit at a size where the focus footer is visible.
    tui.resize(ROWS, COLS);
    tui.wait_for_text("focus: share");
    tui.quit();
}

#[test]
fn the_host_checks_a_device_starts_sharing_sees_the_secrets_and_leaves_it_off() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");
    tui.wait_for_text(&path);

    // The checklist starts empty and is saved into a running session.
    open_settings(&mut tui);
    tui.wait_for_text("Share address");
    tui.wait_for_text(&path);
    assert!(
        tui.text().contains("[ ]"),
        "nothing is checked by default:\n{}",
        tui.text()
    );
    check_device(&mut tui, &path);
    set_port_zero(&mut tui);
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    tui.wait_for_text("focus: stream");

    // The saved file holds preferences only, never a credential.
    let saved = fixture.settings_text();
    assert!(saved.contains("\"share_port\": 0"), "the saved port is 0");
    assert!(
        !saved.contains("credential"),
        "the settings file holds no secret"
    );

    // The sidebar's Share button starts the session deliberately.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    click_text(&mut tui, "[ Share: off ]");
    tui.wait_for_text("focus: share");
    tui.wait_until("the Share page to show its explicit Start button", |text| {
        text.contains("[ Start sharing ]")
    });
    assert!(
        !tui.text().contains("Human terminal key"),
        "opening the page starts nothing:\n{}",
        tui.text()
    );
    start_sharing(&mut tui);
    tui.wait_for_text("Firewall and router settings");
    // The normal view shows two distinct, labelled sharing keys.
    tui.wait_for_text("Human terminal key");
    tui.wait_for_text("MCP/AI key");
    tui.wait_until("the human key prefix on screen", |text| {
        text.contains("snx1.human.")
    });
    // The complete human key is long; the AI key follows below it and is
    // reachable by scrolling. The copy buttons above act on the complete
    // strings, not on what happens to be visible.
    assert!(
        !fixture.settings_text().contains("snx1."),
        "the settings file never holds a sharing key"
    );

    // A hard-wrapped key cannot be selected without line breaks, so the
    // explicit buttons ask the terminal to copy the complete strings.
    activate_share_button(&mut tui, "[ Copy human key ]");
    wait_for_copied_key(&mut tui, "snx1.human.");
    activate_share_button(&mut tui, "[ Copy MCP/AI key ]");
    wait_for_copied_key(&mut tui, "snx1.ai.");
    let copied = copied_keys(&tui.raw_transcript());
    assert!(copied.len() >= 2, "both keys were offered for copying");
    assert!(
        copied.iter().all(|key| key.len() > 300),
        "a copied key is the complete string, not a wrapped fragment"
    );
    assert_ne!(
        copied.iter().find(|key| key.starts_with("snx1.human.")),
        copied.iter().find(|key| key.starts_with("snx1.ai."))
    );

    // When the terminal ignores OSC 52, `v` shows the complete key as one
    // logical line that the terminal soft-wraps; copying that line does not
    // add line breaks. Any key returns to the Share page.
    let human = copied
        .iter()
        .find(|key| key.starts_with("snx1.human."))
        .expect("the human key was offered")
        .clone();
    activate_share_button(&mut tui, "[ Show human key ]");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let raw = tui.raw_transcript();
        if raw
            .windows(human.len())
            .any(|window| window == human.as_bytes())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the soft-wrapped key was not shown"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Mouse motion must not dismiss the key before a selection is copied:
    // the app gives the mouse to the terminal while this view is open.
    let before_motion = tui.raw_transcript().len();
    tui.send(b"\x1b[<35;10;5M");
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        tui.raw_transcript().len(),
        before_motion,
        "mouse motion leaves the key view open"
    );
    // Shrinking the pane while the key is shown must not clip it either.
    let before_resize = tui.raw_transcript().len();
    tui.resize(10, 40);
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !tui.raw_transcript()[before_resize..]
            .windows(human.len())
            .any(|window| window == human.as_bytes()),
        "a shrunken pane does not redraw a clipped key"
    );
    tui.resize(ROWS, COLS);
    std::thread::sleep(Duration::from_millis(200));
    tui.send_text("x");
    tui.wait_for_text("focus: share");
    tui.wait_for_text("Human terminal key");

    // The explicit Advanced row shows the older host fields a terminal without keys needs.
    activate_share_button(&mut tui, "[ Older host fields ]");
    tui.wait_for_text("Older host fields");
    tui.wait_for_text("certificate SHA-256");
    // The CA certificate is long: the separate credentials are below it.
    // The wheel scrolls the details three lines at a time, so the two nearby
    // labels can be brought on screen together.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let text = tui.text();
        if text.contains("human credential") && text.contains("MCP (AI) credential") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the separate credentials are reachable by scrolling"
        );
        tui.send(b"\x1b[<65;50;20M");
        std::thread::sleep(Duration::from_millis(20));
    }
    activate_share_button(&mut tui, "[ Hide older host fields ]");
    tui.wait_for_text("Human terminal key");
    assert!(
        !fixture.settings_text().contains("credential"),
        "the settings file never holds a pairing secret"
    );

    // A pane too short for the whole soft-wrapped line is refused rather
    // than clipped or written to disk. Enlarging it makes v work again.
    tui.resize(10, 40);
    tui.wait_until("the Share page redrawn at a narrow width", |text| {
        !text.contains("Esc back")
    });
    let narrow_offset = tui.raw_transcript().len();
    activate_share_button(&mut tui, "[ Show human key ]");
    tui.wait_for_text("pane is too short");
    assert!(
        !tui.raw_transcript()[narrow_offset..]
            .windows(human.len())
            .any(|window| window == human.as_bytes()),
        "a too-small pane shows no clipped key"
    );
    tui.resize(ROWS, COLS);
    tui.wait_for_text("focus: share");
    activate_share_button(&mut tui, "[ Show human key ]");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !tui.raw_transcript()[narrow_offset..]
        .windows(human.len())
        .any(|window| window == human.as_bytes())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the whole key is shown after enlarging"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    tui.send_text("x");
    tui.wait_for_text("focus: share");

    // The Share page survives a narrow resize; Stop asks for confirmation.
    tui.resize(ROWS, COLS);
    tui.wait_for_text("focus: share");
    stop_sharing(&mut tui);
    tui.wait_for_text("[ Share: off ]");

    // Nothing listens after the terminal leaves.
    assert!(
        !fixture.settings_text().contains("credential"),
        "no secret was written on the way out"
    );
    tui.quit();
}

#[test]
fn a_bind_failure_is_shown_and_sharing_stays_off() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().unwrap().port();
    let mut tui = fixture.tui();
    tui.wait_for_text(&path);

    open_settings(&mut tui);
    check_device(&mut tui, &path);
    set_port(&mut tui, port);
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");

    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    click_text(&mut tui, "[ Share: off ]");
    tui.wait_for_text("focus: share");
    start_sharing(&mut tui);
    tui.wait_for_text("could not bind");
    assert!(
        tui.text().contains("[ Share: off ]"),
        "sharing remains off:\n{}",
        tui.text()
    );
    assert!(
        !tui.text().contains("Human terminal pairing"),
        "no pairing information is shown for a failed bind"
    );

    tui.quit();
}

#[test]
fn cancel_discards_the_checklist_and_the_share_button_refuses_an_empty_one() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text(&path);

    open_settings(&mut tui);
    check_device(&mut tui, &path);
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");
    open_settings(&mut tui);
    tui.wait_until("the cancelled checkbox to be empty", |text| {
        text.lines()
            .any(|line| line.contains("[ ]") && line.contains(&path))
    });
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");

    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    click_text(&mut tui, "[ Share: off ]");
    tui.wait_for_text("focus: share");
    start_sharing(&mut tui);
    tui.wait_for_text("check at least one device");
    assert!(tui.text().contains("[ Share: off ]"));

    tui.quit();
}

#[test]
fn quitting_the_owner_terminal_closes_the_listener_without_stopping_first() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let reserved = TcpListener::bind(("127.0.0.1", 0)).expect("reserve a port");
    let port = reserved.local_addr().expect("addr").port();
    drop(reserved);

    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");
    tui.wait_for_text(&path);
    open_settings(&mut tui);
    check_device(&mut tui, &path);
    set_port(&mut tui, port);
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    click_text(&mut tui, "[ Share: off ]");
    tui.wait_for_text("focus: share");
    start_sharing(&mut tui);
    tui.wait_for_text("Human terminal key");
    std::net::TcpStream::connect(("127.0.0.1", port)).expect("the listener is up");

    // Leave the pairing screen but keep sharing on, then quit through the
    // terminal's normal cleanup path.
    tui.send(KEY_ESC);
    tui.wait_until("the live stream after leaving pairing", |text| {
        text.contains("focus: stream")
    });
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.quit();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Err(_) => break,
            Ok(_) if std::time::Instant::now() >= deadline => {
                panic!("the listener survived the owner terminal")
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// A large paste often arrives in the same kernel read as the keys typed just
/// before it, with more paste bytes left beyond the 1024-byte read buffer.
/// crossterm's default mio reader used edge-triggered readiness and returned
/// after the first parsed event, stranding the rest until another wakeup; the
/// following Esc was never read and the form stayed open. The `use-dev-tty`
/// reader is level-triggered and drains the terminal, so the whole paste and
/// the Esc that follows are processed without any delay or extra keystroke.
#[test]
fn a_large_paste_and_the_key_after_it_are_processed_without_another_wakeup() {
    let fixture = Fixture::new();
    let mut tui = fixture.tui();
    tui.wait_for_text("focus: sidebar");
    tui.send_text("m");
    tui.wait_for_text("Configure MCP pairing");
    tui.send_text("127.0.0.1");
    tui.send(support::tui::KEY_TAB);
    tui.send_text("7331");
    tui.send(support::tui::KEY_TAB);
    // One write: opener, a key longer than the reader's 1024-byte buffer, end
    // marker. No settle delay.
    let key = format!("snx1.ai.{}", "A".repeat(2_000));
    tui.send(format!("\x1b[200~{key}\x1b[201~").as_bytes());
    // The next input must be read too; otherwise the form never closes.
    tui.send(support::tui::KEY_ESC);
    tui.wait_until("the form to close after the paste", |text| {
        text.contains("focus: sidebar")
    });
    tui.quit();
}

#[test]
fn sidebar_entries_only_open_their_pages_and_keyboard_start_stop_confirms() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text(&path);

    // The sidebar's Share entry opens the page; it does not start a listener.
    tui.send_text("s");
    tui.wait_until("the Share page", |text| text.contains("[ Start sharing ]"));
    assert!(
        !tui.text().contains("Human terminal key"),
        "opening the page shows no credentials:\n{}",
        tui.text()
    );
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");

    // The Connect entry opens its page and connects to nothing.
    tui.send_text("c");
    tui.wait_for_text("Connect · sharing key");
    assert!(
        !tui.text().contains("Remote "),
        "opening the Connect page adds no host:\n{}",
        tui.text()
    );
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: sidebar");

    // Configure a checked device and start from the keyboard: the Start row
    // needs one activation, and a running session's Stop needs two.
    open_settings(&mut tui);
    check_device(&mut tui, &path);
    set_port_zero(&mut tui);
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("s");
    tui.wait_for_text("[ Start sharing ]");
    // Address, Port, the local device, then Start.
    for _ in 0..3 {
        tui.send(KEY_DOWN);
    }
    tui.send(KEY_ENTER);
    tui.wait_for_text("Human terminal key");
    tui.wait_for_text("[ Stop sharing ]");
    tui.send(KEY_ENTER);
    tui.wait_for_text("[ Confirm stop ]");
    assert!(
        tui.text().contains("[ Stop sharing ]") || tui.text().contains("[ Confirm stop ]"),
        "the first press only asks"
    );
    tui.send(KEY_ENTER);
    tui.wait_for_text("sharing stopped");
    tui.wait_for_text("[ Share: off ]");
    tui.quit();
}

#[test]
fn the_connect_page_accepts_a_long_paste_and_clears_only_the_key() {
    let fixture = Fixture::new();
    let mut tui = fixture.tui();
    tui.wait_for_text("focus: sidebar");
    tui.send_text("c");
    tui.wait_for_text("Connect · sharing key");
    tui.send_text("192.0.2.10");
    tui.send(KEY_TAB);
    tui.send_text("7331");
    tui.send(KEY_TAB);
    let key = format!("snx1.human.{}", "B".repeat(2_000));
    tui.send(format!("\x1b[200~{key}\x1b[201~").as_bytes());
    // The long key is masked but present; clearing it keeps the endpoint.
    let (column, row) = tui.find("[ Clear key ]").expect("a Clear key button");
    tui.click(column, row);
    tui.wait_until("the cleared masked key field", |text| {
        text.lines()
            .any(|line| line.contains("Human sharing key") && !line.contains('•'))
    });
    tui.send(KEY_ENTER);
    tui.wait_for_text("sharing key");
    tui.wait_for_text("192.0.2.10");
    assert!(
        !tui.text().contains(&key),
        "the key is never drawn or logged in clear text"
    );
    tui.quit();
}

#[test]
fn an_ordinary_notice_clears_itself_in_the_real_terminal() {
    let fixture = Fixture::new();
    let mut tui = fixture.tui();
    tui.wait_for_text(&fixture.device.path);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("d");
    tui.wait_for_text("highlight an attached device first");
    let deadline = std::time::Instant::now() + Duration::from_secs(9);
    loop {
        if !tui.text().contains("highlight an attached device first") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an ordinary notice clears itself after about six seconds:\n{}",
            tui.text()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    tui.quit();
}
