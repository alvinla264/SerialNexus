//! The event-driven app: client calls behind small state transitions that do
//! not depend on a real terminal.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use crossterm::event::MouseEventKind;
use ratatui::layout::Rect;
use serial_nexus_client::{Client, ClientError, RemoteClient, RemoteEndpoint};
use serial_nexus_protocol::{
    DeviceSummary, ErrorCode, Event, OpenResult, PairedHost, PairingRole, PairingSecret, ShareBind,
    SharingKey, WaitSpec, WriteResult,
};

use crate::input::{echo_key, map_key, Action, Key};
use crate::render::{
    connect_hit_test, hit_test, panes, settings_hit_test, share_hit_test, share_max_scroll,
    ConnectHit, Hit, SettingsHit,
};
use crate::settings::{SettingsStore, TerminalSettings};
use crate::state::{
    short_holder, AppState, ClipboardCopy, ConnectForm, DeviceEntry, EndpointId, Focus, FormMode,
    McpPairForm, Notice, SessionKey, SidebarButton, View, LOCAL_ENDPOINT,
};

/// Why saving an MCP pairing stopped.
enum McpSave {
    /// The endpoint is already paired and the person has not confirmed yet.
    NeedsConfirmation,
    /// A clear, secret-free reason shown in the form.
    Failed(String),
}

/// Parse a port field into 1..=65535.
fn parse_port(text: &str) -> Result<u16, String> {
    let port: u16 = text
        .trim()
        .parse()
        .map_err(|_| "the port is not a number from 1 to 65535".to_string())?;
    if port == 0 {
        return Err("the port must be between 1 and 65535".to_string());
    }
    Ok(port)
}

/// A disconnected or revoked remote session no longer belongs in the sidebar.
fn remote_session_is_gone(error: &ClientError) -> bool {
    error.is_device_gone()
        || matches!(error, ClientError::Server(body) if body.code == ErrorCode::PermissionDenied)
}

/// Two clicks on the same cell within this long count as a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// The device operations both transports share, so endpoint policy is chosen
/// once instead of in every close/write/open site.
trait EndpointTransport {
    fn open(
        &self,
        path: &str,
        baud: Option<u32>,
        request_control: bool,
    ) -> Result<OpenResult, ClientError>;
    fn close(&self, device_id: u64) -> Result<(), ClientError>;
    fn write_bytes(
        &self,
        device_id: u64,
        bytes: &[u8],
        wait: Option<WaitSpec>,
    ) -> Result<WriteResult, ClientError>;
}

impl EndpointTransport for Client {
    fn open(
        &self,
        path: &str,
        baud: Option<u32>,
        request_control: bool,
    ) -> Result<OpenResult, ClientError> {
        Client::open(self, path, baud, request_control)
    }

    fn close(&self, device_id: u64) -> Result<(), ClientError> {
        Client::close(self, device_id)
    }

    fn write_bytes(
        &self,
        device_id: u64,
        bytes: &[u8],
        wait: Option<WaitSpec>,
    ) -> Result<WriteResult, ClientError> {
        Client::write_bytes(self, device_id, bytes, wait)
    }
}

impl EndpointTransport for RemoteClient {
    fn open(
        &self,
        path: &str,
        baud: Option<u32>,
        request_control: bool,
    ) -> Result<OpenResult, ClientError> {
        RemoteClient::open(self, path, baud, request_control)
    }

    fn close(&self, device_id: u64) -> Result<(), ClientError> {
        RemoteClient::close(self, device_id)
    }

    fn write_bytes(
        &self,
        device_id: u64,
        bytes: &[u8],
        wait: Option<WaitSpec>,
    ) -> Result<WriteResult, ClientError> {
        RemoteClient::write_bytes(self, device_id, bytes, wait)
    }
}

/// What the first click of a possible double-click landed on. It is kept by
/// identity, not by cell: the first click can move the sidebar's viewport, so
/// hit-testing the second click at the same cell would find another row.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ClickTarget {
    /// A device by endpoint and path. The endpoint keeps equal paths on two
    /// hosts apart, so a double-click never opens the wrong one.
    Device(EndpointId, String),
    Share,
    Connect,
    Settings,
}

struct LastClick {
    at: Instant,
    column: u16,
    row: u16,
    target: Option<ClickTarget>,
}

/// One connected remote host. The link's own client and event stream keep its
/// sessions and failures apart from every other host.
struct RemoteLink {
    id: EndpointId,
    endpoint: RemoteEndpoint,
    client: RemoteClient,
    events: Receiver<Event>,
    critical_event_overflow: Arc<AtomicBool>,
    expected_closes: HashSet<u64>,
}

/// Wire the app to a connected core. The core is started by
/// [`Client::connect`] (the auto-start seam), not by the TUI.
pub struct App {
    pub state: AppState,
    client: Client,
    events: Receiver<Event>,
    /// Closures this terminal asked for; their `DeviceClosed` is not an unplug.
    expected_closes: HashSet<u64>,
    /// Where a changed setting is saved, when this launch has a settings file.
    settings_store: Option<SettingsStore>,
    /// The last click, for recognising a double-click on a device row.
    last_click: Option<LastClick>,
    /// Remote hosts this terminal connected to, each with its own transport.
    remotes: Vec<RemoteLink>,
    /// The next endpoint number a remote link gets.
    next_endpoint: u64,
    /// Where the owner-only MCP pairing file lives, if the environment names
    /// one. Only the explicit MCP pairing action writes it.
    peers_path: Option<PathBuf>,
}

impl App {
    pub fn new(client: Client, rows: usize, cols: usize) -> Self {
        Self::with_settings(client, rows, cols, TerminalSettings::default())
    }

    /// Like [`App::new`], with settings loaded for this launch.
    pub fn with_settings(
        client: Client,
        rows: usize,
        cols: usize,
        settings: TerminalSettings,
    ) -> Self {
        Self::with_preferences(client, rows, cols, settings, None)
    }

    /// Like [`App::with_settings`], and a changed setting is written back
    /// through `store`. Without a store the change still applies to this launch
    /// and is not persisted.
    pub fn with_preferences(
        client: Client,
        rows: usize,
        cols: usize,
        settings: TerminalSettings,
        store: Option<SettingsStore>,
    ) -> Self {
        let events = client.subscribe();
        let mut state = AppState::new(rows, cols);
        state.settings = settings;
        state.draft = state.settings.clone();
        let mut app = Self {
            state,
            client,
            events,
            expected_closes: HashSet::new(),
            settings_store: store,
            last_click: None,
            remotes: Vec::new(),
            next_endpoint: 1,
            peers_path: serial_nexus_client::Peers::default_path().ok(),
        };
        app.refresh_devices();
        app
    }

    /// The settings this launch loaded.
    pub fn settings(&self) -> &TerminalSettings {
        &self.state.settings
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Use an explicit MCP pairing file (tests, embedding). Without this the
    /// per-user default path is used, when the environment names one.
    pub fn set_peers_path(&mut self, path: impl Into<PathBuf>) {
        self.peers_path = Some(path.into());
    }

    /// The MCP pairing file this launch would write, if any.
    pub fn peers_path(&self) -> Option<&std::path::Path> {
        self.peers_path.as_deref()
    }

    /// Re-read every connected host's device list. A remote host that fails
    /// is dropped with its own sessions only; local devices and other remote
    /// hosts keep working.
    pub fn refresh_devices(&mut self) {
        let mut failures = Vec::new();
        match self.client.list_devices() {
            Ok(result) => {
                let entries: Vec<DeviceEntry> =
                    result.devices.into_iter().map(device_entry).collect();
                self.state.set_endpoint_devices(LOCAL_ENDPOINT, entries);
                self.state
                    .reconcile_share_checks(&self.state.detected_local_identities());
            }
            Err(error) => failures.push(error.to_string()),
        }
        failures.extend(self.refresh_remotes());
        if !failures.is_empty() {
            self.state.set_notice(Notice::error(failures.join("; ")));
        }
        if self.state.sharing.active {
            self.sync_share_status();
        }
    }

    /// Re-list the devices of every remote host. A disconnected host is
    /// removed without touching any other endpoint's sessions.
    fn refresh_remotes(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        for link in &self.remotes {
            match link.client.list_devices() {
                Ok(result) => {
                    let entries: Vec<DeviceEntry> = result
                        .devices
                        .into_iter()
                        .map(|device| device_entry_on(link.id, device))
                        .collect();
                    self.state.set_endpoint_devices(link.id, entries);
                }
                Err(error) => failed.push((link.id, error)),
            }
        }
        let mut notices = Vec::with_capacity(failed.len());
        for (id, error) in failed {
            let notice = format!(
                "remote host unreachable: {}: {error}",
                self.state.endpoint_label(id)
            );
            self.drop_remote(id, &notice);
            notices.push(notice);
        }
        notices
    }

    /// Remove one remote host: its sessions, its devices and its link. Other
    /// endpoints are untouched.
    fn drop_remote(&mut self, id: EndpointId, reason: &str) {
        self.state.remove_endpoint(id);
        self.remotes.retain(|link| link.id != id);
        // An open Connect form belongs to a host that is not connected yet, so
        // an unrelated host going away must not close it (and redirect the
        // credentials being typed into the shown device's stream).
        self.state.set_notice(Notice::error(reason.to_string()));
    }

    fn expected_closes_mut(&mut self, endpoint: EndpointId) -> Option<&mut HashSet<u64>> {
        if endpoint == LOCAL_ENDPOINT {
            Some(&mut self.expected_closes)
        } else {
            self.remotes
                .iter_mut()
                .find(|link| link.id == endpoint)
                .map(|link| &mut link.expected_closes)
        }
    }

    /// Drain live events from every host into the state. Events carry their
    /// endpoint, so two hosts with equal device ids never cross streams.
    pub fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.handle_event(LOCAL_ENDPOINT, event);
        }
        let mut disconnected: Vec<EndpointId> = self
            .remotes
            .iter()
            .filter(|link| !link.client.is_connected())
            .map(|link| link.id)
            .collect();
        let overflowed: Vec<EndpointId> = self
            .remotes
            .iter()
            .filter(|link| link.critical_event_overflow.swap(false, Ordering::SeqCst))
            .map(|link| link.id)
            .collect();
        let pending: Vec<(EndpointId, Vec<Event>)> = self
            .remotes
            .iter()
            .map(|link| {
                let mut events = Vec::new();
                loop {
                    match link.events.try_recv() {
                        Ok(event) => events.push(event),
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            disconnected.push(link.id);
                            break;
                        }
                    }
                }
                (link.id, events)
            })
            .collect();
        for (id, events) in pending {
            for event in events {
                self.handle_event(id, event);
            }
        }
        disconnected.sort_unstable();
        disconnected.dedup();
        for id in disconnected {
            if self.remotes.iter().any(|link| link.id == id) {
                self.drop_remote(id, "remote host disconnected: its event stream closed");
            }
        }
        for id in overflowed {
            if self.remotes.iter().any(|link| link.id == id) {
                self.reconcile_remote_after_event_overflow(id);
            }
        }
    }

    fn reconcile_remote_after_event_overflow(&mut self, id: EndpointId) {
        let Some(client) = self
            .remotes
            .iter()
            .find(|link| link.id == id)
            .map(|link| link.client.clone())
        else {
            return;
        };
        if !client.is_connected() {
            self.drop_remote(id, "remote host disconnected: its event stream closed");
            return;
        }
        let sessions: Vec<SessionKey> = self
            .state
            .sessions
            .keys()
            .filter(|key| key.endpoint == id)
            .copied()
            .collect();
        let mut closed = Vec::new();
        let mut failed = Vec::new();
        for key in sessions {
            let path = self
                .state
                .sessions
                .get(&key)
                .map(|session| session.path.clone())
                .unwrap_or_default();
            match client.status(key.device_id) {
                Ok(status) => {
                    if let Some(session) = self.state.sessions.get_mut(&key) {
                        session.control = status.control.holder.as_ref().map(short_holder);
                        session.control_id = status
                            .control
                            .holder
                            .as_ref()
                            .map(|holder| holder.client_id);
                    }
                }
                Err(error) if remote_session_is_gone(&error) => {
                    self.state.remove_session(key);
                    closed.push(path);
                }
                Err(error) => failed.push(format!("{path}: {error}")),
            }
        }
        self.state.set_notice(if !closed.is_empty() {
            Notice::error(format!(
                "remote session closed while its event queue was full: {}",
                closed.join(", ")
            ))
        } else if !failed.is_empty() {
            Notice::error(format!(
                "remote event queue overflowed; session status could not be fully refreshed: {}",
                failed.join("; ")
            ))
        } else {
            Notice::info("remote event queue overflowed; active session status was refreshed")
        });
    }

    fn handle_event(&mut self, endpoint: EndpointId, event: Event) {
        let Some(expected) = self.expected_closes_mut(endpoint) else {
            return;
        };
        if let Event::DeviceClosed { device_id, .. } = &event {
            if expected.remove(device_id) {
                self.state.remove_session(SessionKey {
                    endpoint,
                    device_id: *device_id,
                });
                return;
            }
        }
        if matches!(
            &event,
            Event::TransportClosed { .. } | Event::CoreShutdown { .. }
        ) {
            if endpoint != LOCAL_ENDPOINT {
                let id = endpoint;
                let reason = match &event {
                    Event::TransportClosed { reason } => reason.clone(),
                    Event::CoreShutdown { reason } => reason.clone(),
                    _ => String::new(),
                };
                self.drop_remote(id, &format!("remote host disconnected: {reason}"));
                return;
            }
            // A read failure does not necessarily close the writer socket;
            // explicitly tear down the owner transport so the core observes
            // EOF and revokes the listener and credentials.
            self.client.shutdown_transport();
            self.state.sharing.clear_session();
            self.state.close_share_view();
        }
        self.state.apply_endpoint_event(endpoint, &event);
    }

    /// Handle one key. In the focused stream, keys go to the shown device and
    /// only Ctrl+] leaves; in the sidebar, the command keys act on the
    /// highlighted row and Ctrl+Q quits.
    pub fn handle_key(&mut self, key: Key) {
        // A keyboard action changes the panes under a pending click; the next
        // click must not complete a double-click on a control that moved.
        self.last_click = None;
        let action = map_key(&self.state, key);
        match action {
            Action::Quit => self.state.quit = true,
            Action::Select(delta) => self.select_row(delta),
            Action::OpenSelected => self.activate_selected(),
            Action::DisconnectSelected => self.disconnect_selected(),
            Action::StartBaudPrompt => self.start_baud_prompt(),
            Action::ToggleFocus => self.toggle_focus(),
            Action::PromptChar(ch) => self.state.prompt_push(ch),
            Action::PromptBackspace => self.state.prompt_backspace(),
            Action::PromptCancel => self.state.prompt = None,
            Action::PromptSubmit => self.submit_baud_prompt(),
            Action::PromptCancelFocus => {
                self.state.prompt = None;
                self.toggle_focus();
            }
            Action::Write(bytes) => self.write_shown(&bytes, key),
            Action::Scroll(delta) => self.scroll_stream(delta),
            Action::EnterScrollback => self.enter_scrollback(),
            Action::LeaveScrollback => self.leave_scrollback(),
            Action::OpenSettings => self.open_settings(),
            Action::SettingsMove(delta) => self.state.move_settings_row(delta),
            Action::SettingsToggle => {
                self.state.toggle_draft();
            }
            Action::SettingsBeginEdit => {
                self.state.begin_settings_edit();
            }
            Action::SettingsEditChar(ch) => self.state.settings_edit_push(ch),
            Action::SettingsEditBackspace => self.state.settings_edit_backspace(),
            Action::SettingsEditDone => self.state.end_settings_edit(),
            Action::SettingsSave => self.save_settings(),
            Action::SettingsCancel => self.state.cancel_settings(),
            Action::OpenShare => self.open_share(),
            Action::ShareMove(delta) => self.share_move(delta),
            Action::ShareBeginEdit => {
                self.state.begin_share_edit();
            }
            Action::ShareEditChar(ch) => self.state.share_edit_push(ch),
            Action::ShareEditBackspace => self.state.share_edit_backspace(),
            Action::ShareEditDone => self.state.end_share_edit(),
            Action::ShareToggleDraft => {
                self.state.toggle_share_draft();
            }
            Action::ShareActivate => self.share_activate(),
            Action::ShareSave => self.share_save(),
            Action::ShareCancel => self.share_cancel(),
            Action::ShareScroll(delta) => self.scroll_share(delta),
            Action::DismissNotice => self.dismiss_warning(),
            Action::ConnectClearKey => self.connect_clear_key(),
            Action::ConnectRemote => self.start_connect(),
            Action::ConnectToggleMode => self.connect_toggle_mode(),
            Action::ConnectChar(ch) => self.connect_push(ch),
            Action::ConnectBackspace => self.connect_backspace(),
            Action::ConnectNext(delta) => self.connect_next_field(delta),
            Action::ConnectSubmit => self.submit_connect(),
            Action::ConnectCancel => self.leave_connect(),
            Action::McpPairRemote => self.start_mcp_pair(),
            Action::McpChar(ch) => self.mcp_push(ch),
            Action::McpBackspace => self.mcp_backspace(),
            Action::McpNext(delta) => self.mcp_next_field(delta),
            Action::McpSubmit => self.submit_mcp_pair(),
            Action::McpCancel => self.leave_mcp_pair(),
            Action::PairingToggleAdvanced => self.toggle_pairing_advanced(),
            Action::CopyHumanKey => self.copy_pairing_key(PairingRole::Human),
            Action::CopyAiKey => self.copy_pairing_key(PairingRole::Ai),
            Action::ShowHumanKey => self.show_pairing_key(PairingRole::Human),
            Action::ShowAiKey => self.show_pairing_key(PairingRole::Ai),
            Action::PairingScroll(delta) => self.scroll_share(delta),
            Action::Ignore => {}
        }
    }

    /// Move the keyboard focus between the sidebar and the right pane,
    /// whatever the terminal width. The shown session, the settings draft and
    /// every screen's scroll position stay exactly as they are.
    fn toggle_focus(&mut self) {
        // A right-focused settings view owns its keys (see `map_key`); the
        // sidebar can still be focused to look around, and this brings the
        // keyboard back to the draft.
        self.state.focus = match self.state.focus {
            Focus::Sidebar => Focus::Right,
            Focus::Right => Focus::Sidebar,
        };
    }

    /// Show the highlighted device's kept rows in the right pane. The sidebar's
    /// `b` gets here; a closed device has none and `b` asks for its baud rate
    /// instead, and with the scrollback setting off there is nothing to show.
    fn enter_scrollback(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        let Some(session) = self.state.highlighted_session() else {
            self.state.set_notice(Notice::error(
                "highlight an attached device first: press Enter to open it",
            ));
            return;
        };
        let key = SessionKey {
            endpoint: session.endpoint,
            device_id: session.device_id,
        };
        if !self.state.settings.scrollback {
            self.state
                .set_notice(Notice::error("scrollback is off in the settings"));
            return;
        }
        self.state.show_device(key);
        if let Some(session) = self.state.shown_session_mut() {
            session.screen.scroll_to_bottom();
        }
        self.state.view = View::Scrollback;
        self.state.focus = Focus::Right;
        self.state.clear_ordinary_notice();
    }

    /// Leave the scrollback view for the live stream. The stream starts at the
    /// live screen, so its keys reach the device from where they land.
    fn leave_scrollback(&mut self) {
        self.show_live_stream();
    }

    /// Show the shown device's live stream, never a scrolled-back view. Every
    /// route into the stream goes through here: the stream forwards keys to the
    /// device, so what is drawn has to be where they land.
    fn show_live_stream(&mut self) {
        if let Some(session) = self.state.shown_session_mut() {
            session.screen.scroll_to_bottom();
        }
        self.state.view = View::Stream;
        self.state.focus = Focus::Right;
    }

    /// Show one attached session (a test seam for a user switching streams).
    pub fn show_session_now(&mut self, key: SessionKey) {
        self.focus_session(key);
    }

    fn focus_session(&mut self, key: SessionKey) {
        // A device opened or joined by mouse or keyboard is the surface the
        // human now works on; a baud prompt for another row must not keep the
        // keys.
        self.state.prompt = None;
        self.state.show_device(key);
        self.show_live_stream();
    }

    /// Scroll the scrollback view: positive goes back into the kept output,
    /// negative returns towards live.
    fn scroll_stream(&mut self, delta: i32) {
        let Some(session) = self.state.shown_session_mut() else {
            return;
        };
        if delta > 0 {
            session.screen.scroll_back(delta as usize);
        } else {
            session.screen.scroll_forward(delta.unsigned_abs() as usize);
        }
    }

    /// One mouse-wheel notch in the scrollback view: three lines back or
    /// forward. The live stream has its own keys, so the wheel does nothing
    /// there.
    pub fn handle_scroll(&mut self, up: bool) {
        const NOTCH: i32 = 3;
        match self.state.view {
            View::Scrollback => self.scroll_stream(if up { NOTCH } else { -NOTCH }),
            // The Share page's keys and older host fields can be longer than
            // the pane; the wheel scrolls them without moving the selection.
            View::Share => self.scroll_share(if up { -NOTCH } else { NOTCH }),
            _ => {}
        }
    }

    /// A wheel notch follows the pointer on the Share page: over the form it
    /// moves the selection so off-screen controls are mouse-reachable, over
    /// the details it scrolls the keys.
    fn handle_mouse_scroll(&mut self, area: Rect, column: u16, row: u16, up: bool) {
        if self.state.view == View::Connect {
            if matches!(
                connect_hit_test(&self.state, panes(area).right, column, row),
                Some(ConnectHit::Field(_))
            ) {
                self.connect_next_field(if up { -1 } else { 1 });
            }
            return;
        }
        if self.state.view != View::Share {
            self.handle_scroll(up);
            return;
        }
        match crate::render::share_wheel_target(&self.state, area, column, row) {
            Some(crate::render::ShareWheel::Form) => self.share_move(if up { -1 } else { 1 }),
            Some(crate::render::ShareWheel::Details) => self.scroll_share(if up { -3 } else { 3 }),
            None => {}
        }
    }

    /// Move the sidebar highlight over the devices and the Settings button. A
    /// huge delta (Home/End) lands on the first or last row.
    fn select_row(&mut self, delta: i32) {
        if delta == i32::MIN {
            self.state.select_index(0);
        } else if delta == i32::MAX {
            self.state.select_settings_button();
        } else {
            self.state.select_delta(delta);
        }
    }

    /// Handle one click at one cell of the screen. A sidebar device click only
    /// highlights the row; it never changes the shown stream. The Settings
    /// button opens the settings, and a click in the right pane focuses it.
    pub fn handle_click(&mut self, area: Rect, column: u16, row: u16) {
        let panes = panes(area);
        let in_sidebar = column >= panes.sidebar.x
            && column < panes.sidebar.right()
            && row >= panes.sidebar.y
            && row < panes.sidebar.bottom();
        if panes.split && in_sidebar {
            self.state.focus = Focus::Sidebar;
        }
        match hit_test(&self.state, area, column, row) {
            Some(Hit::Device(index)) => {
                // A click ends a pending baud prompt for another row; opening
                // still takes an Enter or a double-click.
                self.state.prompt = None;
                self.state.focus = Focus::Sidebar;
                self.select_row_index(index);
                return;
            }
            Some(Hit::Share) => {
                self.state.select_sidebar_button(SidebarButton::Share);
                self.open_share();
                return;
            }
            Some(Hit::Connect) => {
                self.state.select_sidebar_button(SidebarButton::Connect);
                self.start_connect();
                return;
            }
            Some(Hit::Dismiss) => {
                self.dismiss_warning();
                return;
            }
            Some(Hit::Settings) => {
                self.open_settings();
                return;
            }
            None => {}
        }
        let right = panes.right;
        let in_right =
            column >= right.x && column < right.right() && row >= right.y && row < right.bottom();
        // In a narrow terminal the single visible pane must already be the
        // right one; clicking the sidebar does not fall through to it.
        if in_right && (panes.split || self.state.focus == Focus::Right) {
            // Clicking the visible right pane takes the keys for it, so a baud
            // prompt must not keep editing a sidebar row in the background.
            self.state.prompt = None;
            self.state.focus = Focus::Right;
            if self.state.view == View::Settings {
                match settings_hit_test(&self.state, right, column, row) {
                    Some(SettingsHit::Flag(index)) if index < self.state.settings_row_count() => {
                        self.state.settings_row = index;
                        self.state.end_settings_edit();
                        match self.state.settings_kind(index) {
                            Some(row) if row.is_toggle() => {
                                self.state.toggle_draft();
                            }
                            Some(row) if row.is_text_field() => {
                                self.state.begin_settings_edit();
                            }
                            None => {}
                            _ => unreachable!("settings row classification is exhaustive"),
                        }
                    }
                    Some(SettingsHit::Save) => self.save_settings(),
                    Some(SettingsHit::Cancel) => self.state.cancel_settings(),
                    Some(SettingsHit::Flag(_)) | None => {}
                }
            } else if self.state.view == View::Share {
                if let Some(index) = share_hit_test(&self.state, right, column, row) {
                    if index < self.state.share_rows().len() {
                        self.state.share_row = index;
                        self.state.end_share_edit();
                        self.share_activate();
                    }
                }
            } else if self.state.view == View::Connect {
                match connect_hit_test(&self.state, right, column, row) {
                    Some(ConnectHit::Field(field)) => {
                        if let Some(form) = &mut self.state.connect {
                            // Moving the pointer between fields is not an
                            // edit; keep the validation reason until input
                            // changes or another submit replaces it.
                            form.field = field;
                        }
                    }
                    Some(ConnectHit::Connect) => self.submit_connect(),
                    Some(ConnectHit::ClearKey) => self.connect_clear_key(),
                    Some(ConnectHit::Cancel) => self.leave_connect(),
                    Some(ConnectHit::ToggleMode) => self.connect_toggle_mode(),
                    None => {}
                }
            }
        }
    }

    /// Handle a mouse event, recognising a double-click on a device row as
    /// "open or join this device". Two clicks on the same cell within
    /// `DOUBLE_CLICK` count; a single click only highlights.
    pub fn handle_mouse(
        &mut self,
        area: Rect,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        at: Instant,
    ) {
        match kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                let double = self.last_click.as_ref().is_some_and(|last| {
                    last.target.is_some()
                        && last.column == column
                        && last.row == row
                        && at.saturating_duration_since(last.at) < DOUBLE_CLICK
                });
                if double {
                    let target = self.last_click.take().and_then(|last| last.target);
                    // A device needs the second click to open it, and Settings
                    // is idempotent. Share and Connect acted on the first
                    // click; running them again would undo or reset it.
                    if !matches!(
                        target,
                        Some(ClickTarget::Share) | Some(ClickTarget::Connect)
                    ) {
                        self.activate_target(target);
                    }
                } else {
                    // Remember what this click landed on before it moves the
                    // sidebar's viewport; the second click of a double-click
                    // must open that same row, not whatever moved under it.
                    let target = self.click_target(area, column, row);
                    self.last_click = Some(LastClick {
                        at,
                        column,
                        row,
                        target,
                    });
                    self.handle_click(area, column, row);
                }
            }
            MouseEventKind::ScrollUp => self.handle_mouse_scroll(area, column, row, true),
            MouseEventKind::ScrollDown => self.handle_mouse_scroll(area, column, row, false),
            _ => {}
        }
    }

    /// What a click at one cell would open on a double-click.
    fn click_target(&self, area: Rect, column: u16, row: u16) -> Option<ClickTarget> {
        match hit_test(&self.state, area, column, row) {
            Some(Hit::Device(index)) => self
                .state
                .devices
                .get(index)
                .map(|device| ClickTarget::Device(device.endpoint, device.path.clone())),
            Some(Hit::Share) => Some(ClickTarget::Share),
            Some(Hit::Connect) => Some(ClickTarget::Connect),
            Some(Hit::Settings) => Some(ClickTarget::Settings),
            Some(Hit::Dismiss) => None,
            None => None,
        }
    }

    /// A double-click on a device row opens or joins it; on the Settings button
    /// it opens the settings; anywhere else the first click already did all
    /// there is to do (the right pane took the focus).
    pub fn handle_double_click(&mut self, area: Rect, column: u16, row: u16) {
        let target = self.click_target(area, column, row);
        self.activate_target(target);
    }

    fn activate_target(&mut self, target: Option<ClickTarget>) {
        match target {
            Some(ClickTarget::Device(endpoint, path)) => {
                self.state.prompt = None;
                self.state.focus = Focus::Sidebar;
                if let Some(index) = self
                    .state
                    .devices
                    .iter()
                    .position(|device| device.endpoint == endpoint && device.path == path)
                {
                    self.state.select_index(index);
                    self.open_selected();
                }
            }
            Some(ClickTarget::Settings) => self.open_settings(),
            Some(ClickTarget::Share) => self.open_share(),
            Some(ClickTarget::Connect) => self.start_connect(),
            // The first click already did all there is to do (focus a pane).
            None => {}
        }
    }

    /// Point the sidebar highlight at one of its rows (a mouse click or a
    /// test). This never changes which device is shown in the right pane.
    pub fn select_row_index(&mut self, index: usize) {
        self.state.select_index(index);
    }

    /// Open the settings in the right pane. The current settings are copied
    /// into a draft, so Escape can throw every change away; reopening settings
    /// that are already open keeps the draft.
    pub fn open_settings(&mut self) {
        self.state.begin_settings();
    }

    /// Refuse a device-opening attempt while the settings hold unsaved
    /// changes, reporting why and keeping the draft. Returns whether it did.
    fn refuse_if_unsaved_settings(&mut self) -> bool {
        if self.state.has_unsaved_settings() {
            self.state.set_notice(Notice::error(
                "settings have unsaved changes: Save or Cancel first",
            ));
            return true;
        }
        false
    }

    /// Activate the highlighted sidebar row: a device opens or joins, a button
    /// does what it says.
    fn activate_selected(&mut self) {
        match self.state.selected_button() {
            Some(SidebarButton::Share) => self.open_share(),
            Some(SidebarButton::Connect) => self.start_connect(),
            Some(SidebarButton::Dismiss) => self.dismiss_warning(),
            Some(SidebarButton::Settings) => self.open_settings(),
            None => self.open_selected(),
        }
    }

    /// Save the draft settings and checklist, then keep the draft and go back
    /// to the shown device's live stream.
    ///
    /// The draft becomes the settings in effect only after both the core
    /// accepted the checklist and the file was written. A refused checklist
    /// change or a failed write leaves the options as they were and stays in
    /// the settings, so nothing is claimed that did not happen.
    fn restore_settings_checklists(
        &mut self,
        draft_checked: crate::state::Checklist,
        pending_checked: crate::state::Checklist,
    ) {
        self.state.draft_checked = draft_checked;
        self.state.sharing.pending_checked = pending_checked;
    }

    fn save_settings(&mut self) {
        self.state.end_settings_edit();
        match self.persist_draft() {
            Ok(()) => {
                self.state.commit_settings();
                self.state.info("settings saved");
            }
            Err(message) if message.contains("unknown") => {
                self.state.warning(format!("settings not saved: {message}"))
            }
            Err(message) => self.state.error(format!("settings not saved: {message}")),
        }
    }

    /// Validate, stage, publish and commit the settings draft and its device
    /// checklist. Nothing becomes effective unless the whole sequence
    /// succeeds; a failure restores both checklists and leaves the draft
    /// exactly as the human typed it. The caller decides whether to keep the
    /// page open and how to show the reason.
    fn persist_draft(&mut self) -> Result<(), String> {
        let address_text = self.state.draft.share_address.trim().to_string();
        let (address, scope) = RemoteEndpoint::parse_address(&address_text)?;
        let port: u16 = self
            .state
            .port_text
            .trim()
            .parse()
            .map_err(|_| "the share port is not a number from 0 to 65535".to_string())?;
        let bind = ShareBind::with_scope(address, port, scope);
        bind.validate()?;
        self.state.draft.share_address = address_text;
        self.state.draft.share_port = port;

        // A temporary loss of identity must not turn an unrelated settings
        // save into an implicit removal from the core's active allowlist. The
        // human can retry after discovery recovers, or explicitly uncheck it.
        if let Some(snapshot) = self
            .state
            .sharing
            .snapshot
            .as_ref()
            .filter(|_| self.state.sharing.active)
        {
            let detected = self.state.detected_local_identities();
            let expected = self.state.draft_checked.identities();
            let unverified_checked = snapshot.checked.iter().any(|device| {
                self.state.draft_checked.contains(&device.path)
                    && !matches!(
                        (detected.get(&device.path), expected.get(&device.path)),
                        (Some(Some(current)), Some(recorded)) if current == recorded
                    )
            });
            if unverified_checked {
                return Err(
                    "cannot verify an active shared device identity; refresh discovery and try again, or uncheck that device"
                        .to_string(),
                );
            }
        }

        // Preserve the human's draft for every failed save, even if current
        // device discovery requires pruning it before authorization.
        let original_draft_checked = self.state.draft_checked.clone();
        let original_pending_checked = self.state.sharing.pending_checked.clone();
        // A replacement at a reused path must be checked again; stale UI state
        // is never turned into an authorization.
        self.state.drop_replaced_checks();

        let draft = self.state.draft.clone();
        let draft_checked = self.state.draft_checked.clone();
        let Some(store) = self.settings_store.clone() else {
            self.restore_settings_checklists(original_draft_checked, original_pending_checked);
            return Err("no settings file".to_string());
        };
        let active = self.state.sharing.active;
        let desired: Vec<String> = draft_checked.iter().cloned().collect();

        // drop_replaced_checks reconciles every checked path against current
        // detection, so this map contains identities captured for surviving
        // paths only.
        let expected = self.state.draft_checked.identities();

        let mut staged = Some(match store.stage(&draft) {
            Ok(staged) => staged,
            Err(error) => {
                self.restore_settings_checklists(
                    original_draft_checked.clone(),
                    original_pending_checked.clone(),
                );
                return Err(error.to_string());
            }
        });

        let mut prepared = None;
        if active {
            match self
                .client
                .prepare_shared_devices_expected(desired.clone(), expected)
            {
                Ok(token) => prepared = Some(token),
                Err(error) => {
                    if let Some(staged) = staged.take() {
                        store.discard(staged);
                    }
                    self.restore_settings_checklists(
                        original_draft_checked.clone(),
                        original_pending_checked.clone(),
                    );
                    return Err(error.to_string());
                }
            }
        }

        // Publish the preferences before committing the checklist, but keep an
        // exact rollback copy until the core confirms. A filesystem failure
        // must never detach sessions for a checklist whose Save failed.
        let mut published = match store.publish_reversibly(staged.take().expect("staged settings"))
        {
            Ok(published) => Some(published),
            Err(error) => {
                self.restore_settings_checklists(
                    original_draft_checked.clone(),
                    original_pending_checked.clone(),
                );
                return Err(error.to_string());
            }
        };

        let mut committed_checklist = false;
        if let Some(token) = prepared {
            match self.client.commit_shared_devices(token) {
                Ok(snapshot) => {
                    let _ = self.state.apply_sharing_snapshot(snapshot);
                    committed_checklist = true;
                }
                Err(error) => {
                    let uncertain = matches!(
                        error,
                        ClientError::Timeout(_) | ClientError::WriteUncertain(_)
                    );
                    if uncertain {
                        self.sync_share_status();
                        committed_checklist = self.state.sharing.active
                            && draft_checked == self.state.sharing.checked();
                    }
                    if !committed_checklist {
                        let rollback = published
                            .take()
                            .expect("published settings before core commit")
                            .rollback();
                        self.state.draft_checked = original_draft_checked.clone();
                        if !uncertain {
                            self.state.sharing.pending_checked = original_pending_checked.clone();
                        }
                        return Err(match rollback {
                            Ok(()) if uncertain => format!(
                                "settings save is unknown: {error}; the core may have applied the checklist — check it and save again"
                            ),
                            Ok(()) => error.to_string(),
                            Err(rollback_error) => format!(
                                "settings save is unknown: {error}; previous preferences could not be restored: {rollback_error}"
                            ),
                        });
                    }
                }
            }
        }

        published
            .take()
            .expect("settings publication remains live")
            .confirm();
        if committed_checklist {
            // The accepted snapshot is authoritative; only now replace the
            // user's attempted draft with the core's committed checklist.
            self.state.draft_checked = self.state.sharing.pending_checked.clone();
        }
        Ok(())
    }

    pub fn toggle_sharing(&mut self) {
        if self.state.sharing.active {
            self.stop_sharing();
        } else {
            self.start_sharing();
        }
    }

    /// Turn sharing on deliberately, as the Share page's Start button does.
    /// At least one device must be checked and the saved bind address must be
    /// one concrete IPv4 or IPv6 address; a bind failure is shown and leaves
    /// sharing off.
    pub fn start_sharing(&mut self) {
        self.state.clear_ordinary_notice();
        match self.try_start_sharing() {
            Ok(()) => self
                .state
                .info("sharing on — the Share page shows the keys"),
            Err(message) => self.state.error(format!("sharing not started: {message}")),
        }
    }

    fn try_start_sharing(&mut self) -> Result<(), String> {
        if self.state.sharing.active {
            return Err("sharing is already on".to_string());
        }
        if self.refuse_if_unsaved_settings() {
            return Err("settings have unsaved changes: Save or Cancel first".to_string());
        }
        self.state.drop_replaced_checks();
        let devices: Vec<String> = self.state.sharing.pending_checked.iter().cloned().collect();
        if devices.is_empty() {
            return Err("check at least one device on the Share page before starting".to_string());
        }
        let bind = self.share_bind()?;
        let expected = self.state.sharing.pending_checked.identities();
        match self.client.start_sharing_expected(bind, devices, expected) {
            Ok(info) => {
                self.state.sharing.active = true;
                self.state.sharing.pairing = Some(info);
                self.sync_share_status();
                self.state.view = View::Share;
                self.state.focus = Focus::Right;
                self.state.prompt = None;
                self.state.pairing_scroll = 0;
                self.state.share_confirm = None;
                self.state.share_error = None;
                Ok(())
            }
            Err(error) => {
                if matches!(
                    error,
                    ClientError::Timeout(_) | ClientError::WriteUncertain(_)
                ) {
                    // The core may have accepted ShareStart before the answer
                    // was lost. Keep the stop control available and ask the
                    // core what is really true instead of claiming it is off.
                    self.state.sharing.active = true;
                    self.sync_share_status();
                    if self.state.sharing.active {
                        // The start response is the only copy of the fresh
                        // credentials. If it was lost, stop the session rather
                        // than leave an active listener whose pairing cannot
                        // be shown to its owner.
                        self.stop_sharing();
                    }
                    let message = if self.state.sharing.active {
                        format!(
                            "sharing start is unknown: {error}; pairing information was lost — stop the session from the Share page"
                        )
                    } else {
                        format!(
                            "sharing start response was lost: {error}; the session was stopped because pairing information is unavailable — start sharing again"
                        )
                    };
                    // An uncertain network outcome must stay until dismissed.
                    self.state.warning(message.clone());
                    Err(message)
                } else {
                    self.state.sharing.clear_session();
                    Err(error.to_string())
                }
            }
        }
    }

    /// The saved sharing endpoint, validated as one concrete address.
    fn share_bind(&self) -> Result<ShareBind, String> {
        let (address, scope) =
            RemoteEndpoint::parse_address(self.state.settings.share_address.trim())?;
        let bind = ShareBind::with_scope(address, self.state.settings.share_port, scope);
        bind.validate()?;
        Ok(bind)
    }

    /// Open the form that saves one endpoint's MCP/AI pairing. It writes only
    /// the private pairing file: it never dials, requests control or changes
    /// sharing.
    pub fn start_mcp_pair(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        self.state.prompt = None;
        if let Some(form) = &mut self.state.mcp_pair {
            form.error = None;
            form.confirm_replace = false;
        } else {
            self.state.mcp_pair = Some(McpPairForm::new());
        }
        self.state.view = View::McpPair;
        self.state.focus = Focus::Right;
    }

    fn leave_mcp_pair(&mut self) {
        self.state.mcp_pair = None;
        self.show_live_stream();
        self.state.focus = Focus::Sidebar;
    }

    pub fn mcp_push(&mut self, ch: char) {
        if let Some(form) = &mut self.state.mcp_pair {
            form.push(ch);
        }
    }

    pub fn mcp_backspace(&mut self) {
        if let Some(form) = &mut self.state.mcp_pair {
            form.backspace();
        }
    }

    pub fn mcp_next_field(&mut self, delta: i32) {
        if let Some(form) = &mut self.state.mcp_pair {
            form.next_field(delta);
        }
    }

    /// Save the AI pairing to the owner-only pairing file. An existing
    /// endpoint needs one confirmation; cancel or any failure leaves the file
    /// exactly as it was, including every other endpoint.
    pub fn submit_mcp_pair(&mut self) {
        let Some(mut form) = self.state.mcp_pair.take() else {
            return;
        };
        if form.key_invalid {
            form.error = Some(
                "the key field received an invalid character; clear the field and paste the whole key again"
                    .to_string(),
            );
            self.state.mcp_pair = Some(form);
            return;
        }
        match self.save_mcp_pair(&mut form) {
            Ok((endpoint, path, outcome)) => {
                self.state.mcp_pair = None;
                self.show_live_stream();
                self.state.focus = Focus::Sidebar;
                let action = match outcome {
                    serial_nexus_client::PeerSave::Added => "saved",
                    serial_nexus_client::PeerSave::Replaced => "replaced",
                };
                self.state.set_notice(Notice::info(format!(
                    "MCP pairing {action} for {endpoint} in {}",
                    path.display()
                )));
            }
            Err(McpSave::NeedsConfirmation) => {
                form.error = Some(
                    "this endpoint is already paired; press Enter again to replace it, or Esc to cancel"
                        .to_string(),
                );
                form.confirm_replace = true;
                self.state.mcp_pair = Some(form);
            }
            Err(McpSave::Failed(error)) => {
                form.error = Some(error);
                self.state.mcp_pair = Some(form);
            }
        }
    }

    fn save_mcp_pair(
        &self,
        form: &mut McpPairForm,
    ) -> Result<(RemoteEndpoint, PathBuf, serial_nexus_client::PeerSave), McpSave> {
        let (address, scope) =
            RemoteEndpoint::parse_address(form.address.trim()).map_err(McpSave::Failed)?;
        let port = parse_port(&form.port).map_err(McpSave::Failed)?;
        let key = SharingKey::parse_for(form.key.trim(), PairingRole::Ai)
            .map_err(|error| McpSave::Failed(format!("sharing key: {error}")))?;
        let socket = ShareBind::with_scope(address, port, scope).socket_addr();
        let paired = key.paired_with(socket).map_err(|_| {
            McpSave::Failed(format!(
                "this sharing key is for {}, not {}",
                key.host_name(),
                address
            ))
        })?;
        let endpoint = RemoteEndpoint::from_socket_addr(paired.address).canonical();
        let path = self
            .peers_path
            .clone()
            .ok_or_else(|| {
                McpSave::Failed(
                    "no MCP pairing file location: set XDG_CONFIG_HOME or HOME, or SERIAL_NEXUS_MCP_PEERS"
                        .to_string(),
                )
            })?;
        if !form.confirm_replace {
            match serial_nexus_client::Peers::load_optional(&path) {
                Ok(Some(peers)) if peers.find(&endpoint).is_ok() => {
                    return Err(McpSave::NeedsConfirmation)
                }
                Ok(_) => {}
                Err(error) => return Err(McpSave::Failed(error.to_string())),
            }
        }
        let outcome = serial_nexus_client::save_peer(
            &path,
            endpoint,
            paired,
            key.secret().clone(),
            form.confirm_replace,
        )
        .map_err(|error| McpSave::Failed(error.to_string()))?;
        Ok((endpoint, path, outcome))
    }

    /// Insert pasted text where the current view takes typing. In the stream
    /// it goes to the shown device exactly as typed keys would.
    pub fn handle_paste(&mut self, text: &str) {
        if self.state.prompt.is_some() {
            for ch in text.chars() {
                self.handle_key(Key::Char(ch));
            }
            return;
        }
        match (self.state.focus, self.state.view) {
            (Focus::Right, View::Connect) => self.paste_into_connect(text),
            (Focus::Right, View::McpPair) => {
                if let Some(form) = &mut self.state.mcp_pair {
                    form.push_str(text);
                }
            }
            (Focus::Right, View::Share) if self.state.share_editing => {
                self.state.share_edit_push_str(text);
            }
            (Focus::Right, View::Settings) if self.state.settings_editing => {
                for ch in text.chars().filter(|ch| ch.is_ascii_graphic()) {
                    self.handle_key(Key::Char(ch));
                }
            }
            (Focus::Right, View::Stream) => {
                for ch in text.chars() {
                    self.handle_key(Key::Char(ch));
                }
            }
            _ => {}
        }
    }

    /// Put one complete sharing key, unwrapped, on the terminal's clipboard.
    /// The Share page's key rows are hard-wrapped, so selecting them would add
    /// line breaks; this is the private, user-directed way to move the exact
    /// string. The event loop writes the OSC 52 sequence and clears this.
    pub fn copy_pairing_key(&mut self, role: PairingRole) {
        let Some(pairing) = &self.state.sharing.pairing else {
            self.state
                .set_notice(Notice::error("sharing is off: start it to copy a key"));
            return;
        };
        let secret = match role {
            PairingRole::Human => &pairing.human,
            PairingRole::Ai => &pairing.ai,
        };
        let label = match role {
            PairingRole::Human => "human terminal key",
            PairingRole::Ai => "MCP/AI key",
        };
        match SharingKey::encode(&pairing.host, secret) {
            Ok(key) => {
                self.state.clipboard_copy = Some(ClipboardCopy { label, key });
                self.state.info(format!(
                    "copy requested for the {label} — the terminal may ignore it; use v or V to select the key"
                ));
            }
            Err(error) => {
                self.state.set_notice(Notice::error(format!(
                    "the {label} could not be built: {error}"
                )));
            }
        }
    }

    /// Take the pending clipboard request, if any.
    pub fn take_clipboard_copy(&mut self) -> Option<ClipboardCopy> {
        self.state.clipboard_copy.take()
    }

    /// Show one complete key as a single soft-wrapped line, for a terminal
    /// that ignores the clipboard escape or has it disabled. Selecting a
    /// soft-wrapped line copies it without inserted newlines in terminals that
    /// mark wraps (Windows Terminal, xterm, tmux). A pane too short for the
    /// whole line is refused with a short message: no part of a key is ever
    /// clipped, and nothing is written to disk.
    pub fn show_pairing_key(&mut self, role: PairingRole) {
        let Some(key) = self.pairing_key(role) else {
            return;
        };
        // The copy view uses the whole terminal, not the stream pane, so the
        // fit decision uses the whole terminal too. Outside a TTY (unit
        // tests) fall back to the stored dimensions.
        let (columns, rows) = crossterm::terminal::size()
            .map(|(columns, rows)| (columns as usize, rows as usize))
            .unwrap_or((self.state.cols(), self.state.rows()));
        let key_rows = key.key.len().div_ceil(columns.max(1)) + 4;
        if key_rows > rows {
            self.state.set_notice(Notice::info(format!(
                "this pane is too short to show the whole {}; enlarge the terminal and activate its Show key button again",
                key.label
            )));
            return;
        }
        self.state.show_key = Some(key);
    }

    fn pairing_key(&mut self, role: PairingRole) -> Option<ClipboardCopy> {
        let Some(pairing) = &self.state.sharing.pairing else {
            self.state
                .set_notice(Notice::error("sharing is off: start it to show a key"));
            return None;
        };
        let secret = match role {
            PairingRole::Human => &pairing.human,
            PairingRole::Ai => &pairing.ai,
        };
        let label = match role {
            PairingRole::Human => "human terminal key",
            PairingRole::Ai => "MCP/AI key",
        };
        match SharingKey::encode(&pairing.host, secret) {
            Ok(key) => Some(ClipboardCopy { label, key }),
            Err(error) => {
                self.state.set_notice(Notice::error(format!(
                    "the {label} could not be built: {error}"
                )));
                None
            }
        }
    }

    /// Show or hide the older host fields on the Share page.
    fn toggle_pairing_advanced(&mut self) {
        self.state.pairing_advanced = !self.state.pairing_advanced;
        self.state.pairing_scroll = 0;
    }

    /// Stop sharing, which revokes both credentials and disconnects every
    /// remote client. The Share page stays open so the new state is visible;
    /// the checked selection stays for this launch only.
    pub fn stop_sharing(&mut self) {
        if !self.state.sharing.active {
            self.state.info("sharing is already off");
            return;
        }
        match self.client.stop_sharing() {
            Ok(()) => {
                self.state.sharing.clear_session();
                self.state.share_confirm = None;
                self.state.share_error = None;
                self.state.info("sharing stopped");
            }
            Err(error) => {
                self.state.warning(format!("sharing not stopped: {error}"));
                self.sync_share_status();
            }
        }
    }

    /// Open the owner's Share page: the address, port, device checklist and
    /// explicit Start/Stop controls. Opening it never starts, stops or
    /// connects anything.
    pub fn open_share(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        self.state.clear_ordinary_notice();
        self.state.begin_share();
    }

    fn leave_share(&mut self) {
        self.state.end_share_edit();
        self.state.share_confirm = None;
        self.state.cancel_share_edits();
        self.show_live_stream();
    }

    fn scroll_share(&mut self, delta: i32) {
        let next = self.state.pairing_scroll as i32 + delta;
        let max = share_max_scroll(&self.state);
        self.state.pairing_scroll = next.clamp(0, max as i32) as usize;
    }

    /// Cancel the smallest thing that is pending: an edit, then a
    /// confirmation, then unsaved edits. With nothing pending, Esc returns to
    /// the stream.
    fn share_cancel(&mut self) {
        if self.state.share_editing {
            self.state.end_share_edit();
            return;
        }
        if self.state.share_confirm.is_some() {
            self.state.share_confirm = None;
            return;
        }
        if self.state.has_unsaved_share() {
            self.state.cancel_share_edits();
            return;
        }
        self.leave_share();
    }

    fn dismiss_warning(&mut self) {
        self.state.dismiss_warning();
        if self.state.selected_button() == Some(SidebarButton::Dismiss) {
            self.state.select_settings_button();
        }
    }

    fn share_move(&mut self, delta: i32) {
        self.state.move_share_row(delta);
    }

    /// Activate the selected Share row. Editable rows start editing or toggle;
    /// every network action is an explicit button that may need confirmation.
    fn share_activate(&mut self) {
        match self.state.selected_share_row() {
            crate::state::ShareRow::Address | crate::state::ShareRow::Port => {
                self.state.begin_share_edit();
            }
            crate::state::ShareRow::Device(_) => {
                self.state.toggle_share_draft();
            }
            crate::state::ShareRow::StartStop => self.share_start_or_stop(),
            crate::state::ShareRow::Save => self.share_save(),
            crate::state::ShareRow::Cancel => self.state.cancel_share_edits(),
            crate::state::ShareRow::CopyHuman => self.copy_pairing_key(PairingRole::Human),
            crate::state::ShareRow::CopyAi => self.copy_pairing_key(PairingRole::Ai),
            crate::state::ShareRow::ShowHuman => self.show_pairing_key(PairingRole::Human),
            crate::state::ShareRow::ShowAi => self.show_pairing_key(PairingRole::Ai),
            crate::state::ShareRow::Advanced => self.toggle_pairing_advanced(),
        }
    }

    fn share_start_or_stop(&mut self) {
        if self.state.sharing.active {
            if self.state.share_confirm == Some(crate::state::ShareConfirm::Stop) {
                self.state.share_confirm = None;
                self.stop_sharing();
            } else {
                self.state.share_confirm = Some(crate::state::ShareConfirm::Stop);
                self.state.share_error = None;
            }
            return;
        }
        if self.state.has_unsaved_share() {
            if self.state.share_confirm == Some(crate::state::ShareConfirm::Start) {
                self.share_save_and_start();
            } else {
                self.state.share_confirm = Some(crate::state::ShareConfirm::Start);
                self.state.share_error = None;
            }
        } else {
            self.state.share_confirm = None;
            self.share_start_now();
        }
    }

    fn share_start_now(&mut self) {
        match self.try_start_sharing() {
            Ok(()) => self.state.info("sharing on — copy a key below"),
            Err(message) => self.state.share_error = Some(message),
        }
    }

    fn share_save(&mut self) {
        self.state.end_share_edit();
        let was_active = self.state.sharing.active;
        // Compare the parsed endpoints, not the raw address text: canonical
        // IPv6 spellings and interface scopes must not look like a change.
        let previous_bind = self.share_bind().ok();
        let endpoint_changed = match (previous_bind, self.draft_share_bind().ok()) {
            (Some(previous), Some(draft)) => previous != draft,
            _ => true,
        };
        if was_active && endpoint_changed {
            // A listener cannot move without closing and reissuing every
            // credential. Refusing here keeps the running session, its
            // credentials and its remote clients exactly as they were; the
            // human stops sharing first when they really want a new endpoint.
            self.share_save_failed(
                "stop sharing before changing the address or port; the running share is unchanged"
                    .to_string(),
            );
            return;
        }

        match self.persist_draft() {
            Ok(()) => {
                self.state.commit_share_draft();
                self.state.info("share settings saved");
            }
            Err(message) => self.share_save_failed(message),
        }
    }

    fn draft_share_bind(&self) -> Result<ShareBind, String> {
        let (address, scope) =
            RemoteEndpoint::parse_address(self.state.draft.share_address.trim())?;
        let port: u16 = self
            .state
            .port_text
            .trim()
            .parse()
            .map_err(|_| "the share port is not a number from 0 to 65535".to_string())?;
        let bind = ShareBind::with_scope(address, port, scope);
        bind.validate()?;
        Ok(bind)
    }

    fn share_save_and_start(&mut self) {
        self.state.end_share_edit();
        match self.persist_draft() {
            Ok(()) => {
                self.state.commit_share_draft();
                self.share_start_now();
            }
            Err(message) => {
                self.state.share_confirm = None;
                self.share_save_failed(message);
            }
        }
    }

    /// Show a failed Share save in its form. An outcome the core may already
    /// have applied is serious: it also becomes a sticky warning so moving the
    /// selection cannot hide it before the human acknowledges it.
    fn share_save_failed(&mut self, message: String) {
        if message.contains("unknown") {
            self.state
                .warning(format!("share settings not saved: {message}"));
        }
        self.state.share_error = Some(message);
    }

    /// Ask a remote host for its address, port and pairing information. The
    /// form's values stay in memory; nothing is written to the settings file
    /// or sent to a serial device.
    pub fn start_connect(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        self.state.clear_ordinary_notice();
        self.state.prompt = None;
        if let Some(form) = &mut self.state.connect {
            form.error = None;
        } else {
            self.state.connect = Some(ConnectForm::new());
        }
        self.state.view = View::Connect;
        self.state.focus = Focus::Right;
    }

    fn leave_connect(&mut self) {
        self.state.connect = None;
        self.show_live_stream();
        self.state.focus = Focus::Sidebar;
    }

    /// Switch the open Connect form to or from the older-host fields. The
    /// typed address and port carry over; nothing is submitted or connected.
    pub fn connect_toggle_mode(&mut self) {
        if let Some(form) = &mut self.state.connect {
            form.toggle_mode();
        }
    }

    pub fn connect_push(&mut self, ch: char) {
        if let Some(form) = &mut self.state.connect {
            form.push(ch);
        }
    }

    /// Insert a pasted string into the focused form field in one action.
    fn paste_into_connect(&mut self, text: &str) {
        if let Some(form) = &mut self.state.connect {
            form.push_str(text);
        }
    }

    pub fn connect_backspace(&mut self) {
        if let Some(form) = &mut self.state.connect {
            form.backspace();
        }
    }

    /// Clear the complete human sharing key while keeping the endpoint.
    pub fn connect_clear_key(&mut self) {
        if let Some(form) = &mut self.state.connect {
            form.clear_key();
        }
    }

    pub fn connect_next_field(&mut self, delta: i32) {
        if let Some(form) = &mut self.state.connect {
            form.next_field(delta);
        }
    }

    /// Authenticate the encrypted host and add its approved devices. A wrong
    /// certificate, address, port or credential keeps the form open with the
    /// reason and never silently trusts the host.
    pub fn submit_connect(&mut self) {
        let Some(mut form) = self.state.connect.take() else {
            return;
        };
        if form.mode == FormMode::Key && form.key_invalid {
            form.error = Some(
                "the key field received an invalid character; clear the field and paste the whole key again"
                    .to_string(),
            );
            self.state.connect = Some(form);
            return;
        }
        let result = self.connect_remote(&form);
        match result {
            Ok((id, label)) => {
                let existing_notice = self.state.notice.clone();
                self.state.remote_labels.insert(id, label.clone());
                self.state.connect = None;
                self.show_live_stream();
                self.state.focus = Focus::Sidebar;
                let refresh_failures = self.refresh_remotes();
                if self.remotes.iter().any(|link| link.id == id) {
                    if refresh_failures.is_empty() {
                        if existing_notice.is_none() || self.state.notice == existing_notice {
                            self.state.set_notice(Notice::info(format!(
                                "connected to {label}: its checked devices are in the sidebar"
                            )));
                        }
                    } else {
                        let mut notices = refresh_failures;
                        if let Some(Notice { text, .. }) = existing_notice {
                            if !notices.iter().any(|notice| notice == &text) {
                                notices.insert(0, text);
                            }
                        }
                        self.state.set_notice(Notice::error(format!(
                            "connected to {label}; {}",
                            notices.join("; ")
                        )));
                    }
                }
            }
            Err(error) => {
                form.error = Some(error);
                self.state.connect = Some(form);
            }
        }
    }

    /// Dial and authenticate one host, then register its endpoint. Every
    /// failure leaves nothing behind. The normal form carries one sharing key
    /// (host verification and the human credential); the advanced form keeps
    /// the older separate fields for hosts that predate keys.
    fn connect_remote(&mut self, form: &ConnectForm) -> Result<(EndpointId, String), String> {
        let (address, scope) = RemoteEndpoint::parse_address(form.address.trim())?;
        let port = parse_port(&form.port)?;
        let (paired, secret) = match form.mode {
            FormMode::Key => {
                let key = SharingKey::parse_for(form.key.trim(), PairingRole::Human)
                    .map_err(|error| format!("sharing key: {error}"))?;
                let socket = ShareBind::with_scope(address, port, scope).socket_addr();
                let paired = key.paired_with(socket).map_err(|_| {
                    format!(
                        "this sharing key is for {}, not {}",
                        key.host_name(),
                        address
                    )
                })?;
                let secret = key.secret().clone();
                (paired, secret)
            }
            FormMode::Advanced => {
                let fingerprint = form.fingerprint.trim().to_ascii_lowercase();
                if fingerprint.len() != 64 || !fingerprint.chars().all(|ch| ch.is_ascii_hexdigit())
                {
                    return Err("the host fingerprint is not a SHA-256 hex value".to_string());
                }
                if form.ca.trim().is_empty() {
                    return Err("enter the host's CA certificate from its Share page".to_string());
                }
                if form.credential.trim().is_empty() {
                    return Err("enter the host's human pairing credential".to_string());
                }
                let host_name = address.to_string();
                let paired = PairedHost {
                    address: ShareBind::with_scope(address, port, scope).socket_addr(),
                    host_name,
                    ca_cert_der_b64: form.ca.trim().to_string(),
                    host_cert_fingerprint_sha256: fingerprint,
                };
                let secret = PairingSecret {
                    role: PairingRole::Human,
                    credential: form.credential.trim().to_string(),
                };
                (paired, secret)
            }
        };
        let endpoint = RemoteEndpoint::from_socket_addr(paired.address).canonical();
        let existing_id = self
            .remotes
            .iter()
            .find(|link| link.endpoint == endpoint)
            .map(|link| link.id);
        if let Some(existing_id) = existing_id.filter(|existing_id| {
            self.remotes.iter().any(|link| {
                link.id == *existing_id
                    && link.endpoint == endpoint
                    && link.client.is_connected()
                    && link.client.matches_pairing(endpoint, &paired, &secret)
            })
        }) {
            return Ok((existing_id, format!("Remote {endpoint}")));
        }
        let remote_client = serial_nexus_client::RemoteClient::connect_to(
            endpoint,
            &paired,
            &secret,
            Some("serial-nexus-terminal".to_string()),
            Duration::from_secs(5),
        )
        .map_err(|error| format!("not connected: {error}"))?;
        if let Some(existing_id) = existing_id {
            if self.remotes.iter().any(|link| link.id == existing_id) {
                self.state.remove_endpoint(existing_id);
                let (events, critical_event_overflow) =
                    remote_client.subscribe_bounded_with_overflow(1024);
                let link = self
                    .remotes
                    .iter_mut()
                    .find(|link| link.id == existing_id)
                    .expect("existing endpoint link");
                let old_client = std::mem::replace(&mut link.client, remote_client);
                link.events = events;
                link.critical_event_overflow = critical_event_overflow;
                link.expected_closes.clear();
                drop(old_client);
                return Ok((existing_id, format!("Remote {endpoint}")));
            }
        }
        let id = EndpointId(self.next_endpoint);
        self.next_endpoint += 1;
        let label = format!("Remote {}", paired.address);
        let (events, critical_event_overflow) = remote_client.subscribe_bounded_with_overflow(1024);
        self.remotes.push(RemoteLink {
            id,
            endpoint,
            client: remote_client,
            events,
            critical_event_overflow,
            expected_closes: HashSet::new(),
        });
        Ok((id, label))
    }

    fn remote_client(&self, endpoint: EndpointId) -> Option<&RemoteClient> {
        self.remotes
            .iter()
            .find(|link| link.id == endpoint)
            .map(|link| &link.client)
    }

    /// Resolve one endpoint to its transport. `None` means this machine's
    /// client; an unknown remote endpoint is an error everywhere, never a
    /// silent fallback to the local core.
    fn endpoint_client(&self, endpoint: EndpointId) -> Result<&dyn EndpointTransport, ClientError> {
        if endpoint == LOCAL_ENDPOINT {
            return Ok(&self.client);
        }
        self.remote_client(endpoint)
            .map(|remote| remote as &dyn EndpointTransport)
            .ok_or_else(|| {
                ClientError::Disconnected("the remote host is no longer connected".into())
            })
    }

    fn open_on(
        &self,
        endpoint: EndpointId,
        path: &str,
        baud: Option<u32>,
    ) -> Result<serial_nexus_protocol::OpenResult, ClientError> {
        self.endpoint_client(endpoint)?.open(path, baud, false)
    }

    /// Ask the core for the running session's state and take its answer as
    /// authoritative. A checked device the core no longer lists (unplugged,
    /// replaced at the same path, or revoked) loses its checkbox.
    fn sync_share_status(&mut self) {
        match self.client.sharing_status() {
            Ok(snapshot) => {
                let lost = self.state.apply_sharing_snapshot(snapshot);
                if !lost.is_empty() {
                    self.state.set_notice(Notice::error(format!(
                        "no longer shared: {} — check it again in Settings",
                        lost.join(", ")
                    )));
                }
            }
            Err(error) => {
                // A timeout says only that we do not know the state. Keep the
                // owner view (and the credentials in memory) so another `s`
                // still sends ShareStop; never claim the listener is gone.
                let ended = matches!(
                    error,
                    ClientError::Disconnected(_) | ClientError::WriteUncertain(_)
                ) || matches!(
                    error.code(),
                    Some(
                        ErrorCode::NotFound | ErrorCode::PermissionDenied | ErrorCode::ShuttingDown
                    )
                );
                if ended {
                    self.state.sharing.clear_session();
                    self.state.close_share_view();
                    self.state
                        .set_notice(Notice::error(format!("sharing stopped: {error}")));
                } else {
                    self.state.warning(format!(
                        "sharing status is unknown: {error}; sharing may still be on — stop it from the Share page"
                    ));
                }
            }
        }
    }

    /// Open or join the highlighted device and show it in the right pane. A
    /// closed device asks for a baud rate first; an already-open device is
    /// joined at its active rate.
    ///
    /// While the settings hold unsaved changes, opening is refused and the
    /// draft is kept: Save or Cancel is the only way out of the settings.
    pub fn open_selected(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        let Some(device) = self.state.selected_device() else {
            self.state
                .set_notice(Notice::error("no device highlighted"));
            return;
        };
        let path = device.path.clone();
        let endpoint = device.endpoint;
        if let Some(session) = self.state.highlighted_session() {
            let key = SessionKey {
                endpoint: session.endpoint,
                device_id: session.device_id,
            };
            self.focus_session(key);
            self.state.set_notice(Notice::info("already attached"));
            return;
        }
        if device.open {
            match self.open_endpoint(endpoint, &path, None) {
                Ok(_) => {}
                Err(error) => self.state.set_notice(Notice::error(error.to_string())),
            }
        } else if !self.state.begin_baud_prompt() {
            self.state
                .set_notice(Notice::error("cannot open the highlighted device"));
        }
    }

    /// Open or join a device on this machine and show its live stream.
    pub fn open_path(&mut self, path: &str, baud: Option<u32>) -> Result<u64, ClientError> {
        self.open_endpoint(LOCAL_ENDPOINT, path, baud)
            .map(|key| key.device_id)
    }

    /// Open or join a device on one host and show its live stream. Sessions
    /// start empty and are fed by live events only, so output races never
    /// duplicate. The key includes the endpoint, so equal device ids and paths
    /// on two hosts stay separate.
    pub fn open_endpoint(
        &mut self,
        endpoint: EndpointId,
        path: &str,
        baud: Option<u32>,
    ) -> Result<SessionKey, ClientError> {
        let opened = self.open_on(endpoint, path, baud)?;
        // Rejoining a device (even the same id, while another client kept it
        // open) must not leave an old close suppression behind.
        if let Some(expected) = self.expected_closes_mut(endpoint) {
            expected.remove(&opened.device_id);
        }
        let key = SessionKey {
            endpoint,
            device_id: opened.device_id,
        };
        self.state.add_endpoint_session(
            endpoint,
            opened.device_id,
            opened.port.clone(),
            opened.baud,
            opened.control.holder.clone(),
        );
        self.focus_session(key);
        self.state.set_notice(Notice::info(format!(
            "opened {} at {} baud",
            opened.port, opened.baud
        )));
        self.refresh_devices();
        Ok(key)
    }

    fn submit_baud_prompt(&mut self) {
        let Some(prompt) = self.state.prompt.take() else {
            return;
        };
        // A prompt can outlive a clean settings view that then gained a draft
        // (a flag click); the opening attempt is refused like any other, and
        // the draft is kept.
        if self.refuse_if_unsaved_settings() {
            return;
        }
        match prompt.value() {
            Ok(baud) => match self.open_endpoint(prompt.endpoint, &prompt.path, Some(baud)) {
                Ok(_) => {}
                Err(error) => {
                    let mut prompt = prompt;
                    prompt.error = Some(error.to_string());
                    self.state.prompt = Some(prompt);
                }
            },
            Err(message) => {
                let mut prompt = prompt;
                prompt.error = Some(message);
                self.state.prompt = Some(prompt);
            }
        }
    }

    fn start_baud_prompt(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        if self.state.begin_baud_prompt() {
            return;
        }
        let Some(device) = self.state.selected_device() else {
            self.state
                .set_notice(Notice::error("no device highlighted"));
            return;
        };
        if let Some(baud) = device.baud {
            self.state.set_notice(Notice::info(format!(
                "already open at {baud} baud; Enter joins at the active rate"
            )));
        }
    }

    /// Explicitly detach the highlighted device. Other sessions stay
    /// connected, and the shown stream only changes if that device was shown.
    pub fn disconnect_selected(&mut self) {
        let Some(session) = self.state.highlighted_session() else {
            self.state
                .set_notice(Notice::error("highlight an attached device first"));
            return;
        };
        let key = SessionKey {
            endpoint: session.endpoint,
            device_id: session.device_id,
        };
        let path = session.path.clone();
        if let Some(expected) = self.expected_closes_mut(key.endpoint) {
            expected.insert(key.device_id);
        }
        let closed = self
            .endpoint_client(key.endpoint)
            .and_then(|transport| transport.close(key.device_id));
        match closed {
            Ok(()) => {
                self.state.remove_session(key);
                self.state
                    .set_notice(Notice::info(format!("disconnected {path}")));
            }
            Err(error) => {
                if error.is_device_gone() {
                    self.state.remove_session(key);
                }
                self.state
                    .error(format!("disconnect {path} failed: {error}"));
            }
        }
        self.refresh_devices();
    }

    /// Write human input to the shown device without taking AI control. With
    /// `local_echo` on, the typed text is shown locally too; it is added only
    /// after the write was accepted, so a refused or uncertain write leaves no
    /// ghost and is never retried.
    ///
    /// The sidebar highlight plays no part here: typed device input goes only
    /// to the stream that has the focus and is on screen.
    pub fn write_shown(&mut self, bytes: &[u8], key: Key) {
        let Some(session) = self.state.shown_session() else {
            self.state.set_notice(Notice::error(
                "no device shown — highlight one and press Enter to open it",
            ));
            return;
        };
        let session_key = SessionKey {
            endpoint: session.endpoint,
            device_id: session.device_id,
        };
        let path = session.path.clone();
        let endpoint = session.endpoint;
        let written = self
            .endpoint_client(endpoint)
            .and_then(|transport| transport.write_bytes(session_key.device_id, bytes, None));
        if let Err(error) = written {
            if error.is_device_gone() {
                self.state.remove_session(session_key);
            }
            let retried = if error.is_write_uncertain() {
                " — the bytes may have reached it and were not retried"
            } else {
                ""
            };
            let text = format!("write to {path}{retried}: {error}");
            if error.is_write_uncertain() {
                self.state.warning(text);
            } else {
                self.state.error(text);
            }
            return;
        }
        let local_echo = self.state.settings.local_echo;
        if local_echo {
            if let (Some(echo), Some(session)) =
                (echo_key(key), self.state.sessions.get_mut(&session_key))
            {
                session.screen.echo(&echo);
            }
        }
    }

    /// Detach every device and stop sharing. Called on exit and after an
    /// event-loop error, so this terminal never leaves a listener or a remote
    /// session behind.
    pub fn shutdown(&mut self) {
        if self.state.sharing.active {
            let _ = self.client.stop_sharing();
            self.state.sharing.clear_session();
        }
        for key in self.state.all_sessions() {
            if let Ok(transport) = self.endpoint_client(key.endpoint) {
                let _ = transport.close(key.device_id);
            }
            self.state.remove_session(key);
        }
        for link in self.remotes.drain(..) {
            self.state.remove_endpoint(link.id);
            drop(link);
        }
        self.state.remote_labels.clear();
        self.pump();
    }

    pub fn resize_stream(&mut self, rows: usize, cols: usize) {
        self.state.resize(rows, cols);
    }

    /// The terminal was resized: a pending first click must not complete a
    /// double-click on a control that has moved to another cell.
    pub fn handle_resize(&mut self) {
        self.last_click = None;
    }
}

fn device_entry(device: DeviceSummary) -> DeviceEntry {
    device_entry_on(LOCAL_ENDPOINT, device)
}

fn device_entry_on(endpoint: EndpointId, device: DeviceSummary) -> DeviceEntry {
    DeviceEntry {
        endpoint,
        path: device.path,
        description: device.description,
        identity: device.identity,
        open: device.open,
        device_id: device.device_id,
        baud: device.baud,
        control: device.control_holder.as_ref().map(short_holder),
        control_id: device
            .control_holder
            .as_ref()
            .map(|holder| holder.client_id),
    }
}

#[cfg(test)]
mod event_overflow_tests {
    use super::*;

    #[test]
    fn a_revoked_remote_device_is_treated_as_a_gone_session() {
        assert!(remote_session_is_gone(&ClientError::Server(
            serial_nexus_client::ErrorBody::new(ErrorCode::PermissionDenied, "no longer shared")
        )));
        assert!(!remote_session_is_gone(&ClientError::Timeout(
            "slow host".into()
        )));
    }
}
