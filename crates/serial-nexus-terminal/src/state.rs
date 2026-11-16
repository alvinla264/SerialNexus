//! The TUI's state: devices, open sessions, focus, prompt and notices.
//!
//! This module has no terminal or socket code, so the interesting behaviour
//! (switching, filtering human writes, unplug notices) is testable on its own.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use serial_nexus_protocol::{
    Event, Holder, ShareSessionInfo, SharingSnapshot, Source, DEFAULT_BAUD,
};

use crate::screen::{Screen, SCROLLBACK_ROWS};
use crate::settings::TerminalSettings;

/// Which pane receives keys: the device sidebar on the left, or the right-hand
/// pane (the shown device's stream, its scrollback, or the settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Right,
}

/// Which sidebar button the highlight is on, when it is not on a device row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarButton {
    /// Start or stop the opt-in LAN sharing session.
    Share,
    /// Pair with a remote host and connect to it.
    Connect,
    /// Dismiss the visible serious warning. Only shown while one is pending.
    Dismiss,
    /// Open the settings in the right pane.
    Settings,
}

/// What the right-hand pane shows. The sidebar is beside it, never instead of
/// it, except in narrow terminals where only the focused pane is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Stream,
    Settings,
    Scrollback,
    /// The owner's pairing information for the running sharing session. The
    /// secrets in it exist only in memory and are drawn only here.
    Share,
    /// The form that asks for a remote host's address, port and sharing key.
    Connect,
    /// The form that writes one endpoint's MCP/AI pairing into the private
    /// pairing file. It never connects or changes sharing.
    McpPair,
}

/// The settings rows before the detected-device checklist: local echo,
/// scrollback, the sharing address and the sharing port.
pub const SETTINGS_FIXED_ROWS: usize = 4;

/// Fixed settings rows in drawing, navigation and label order.
const FIXED_SETTINGS: [(SettingsRow, &str); SETTINGS_FIXED_ROWS] = [
    (SettingsRow::LocalEcho, "Local echo"),
    (SettingsRow::Scrollback, "Scrollback"),
    (SettingsRow::ShareAddress, "Share address"),
    (SettingsRow::SharePort, "Share port"),
];

/// What one settings row edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsRow {
    LocalEcho,
    Scrollback,
    ShareAddress,
    SharePort,
    /// The checkbox for one detected device, by its index in `devices`.
    Device(usize),
}

impl SettingsRow {
    pub fn is_text_field(self) -> bool {
        matches!(self, Self::ShareAddress | Self::SharePort)
    }

    pub fn is_toggle(self) -> bool {
        !self.is_text_field()
    }
}

/// One row of the owner's Share page. The address, port and device checks are
/// editable; the rest are explicit buttons. No row starts, stops or connects
/// until the human activates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareRow {
    Address,
    Port,
    Device(usize),
    StartStop,
    Save,
    Cancel,
    CopyHuman,
    CopyAi,
    ShowHuman,
    ShowAi,
    Advanced,
}

/// A Share-page action waiting for the human's confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareConfirm {
    /// Start, after saving the edited address, port or device selection.
    Start,
    /// Stop the running session and disconnect every remote client.
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Error,
}

/// How long an ordinary notice stays on screen without being replaced.
pub const NOTICE_TTL: Duration = Duration::from_secs(6);

/// A message shown outside the device stream (errors, unplug, hints).
///
/// Ordinary notices are transient: they clear themselves after
/// [`NOTICE_TTL`]. A serious warning (`sticky`) stays until the human
/// dismisses it from the sidebar, even while ordinary notices arrive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub severity: Severity,
    /// Kept until explicitly dismissed instead of expiring.
    pub sticky: bool,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Info,
            sticky: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Error,
            sticky: false,
        }
    }

    /// A serious warning that must not be lost, overwritten by an ordinary
    /// notice, or auto-dismissed. The human dismisses it from the sidebar.
    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Error,
            sticky: true,
        }
    }

    pub fn is_sticky(&self) -> bool {
        self.sticky
    }
}

/// The checked devices of one checklist, with the identity each path was
/// selected as. One map keeps the path/identity invariant in one place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checklist {
    entries: BTreeMap<String, Option<String>>,
}

impl Checklist {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    pub fn insert(&mut self, path: String) -> bool {
        use std::collections::btree_map::Entry;
        if let Entry::Vacant(entry) = self.entries.entry(path) {
            entry.insert(None);
            true
        } else {
            false
        }
    }

    fn preserve_selected_path(&mut self, path: &str, previous: &Checklist) {
        match previous.entries.get(path) {
            Some(Some(identity)) => {
                self.insert_with_identity(path.to_string(), identity.clone());
            }
            Some(None) => {
                self.insert(path.to_string());
            }
            None => {}
        }
    }

    /// Preserve the terminal's detected identity from the effective selection
    /// or draft. The core snapshot's identity token is a different namespace.
    fn preserve_snapshot_selection(
        &mut self,
        path: &str,
        effective: &Checklist,
        draft: &Checklist,
    ) {
        let previous = if effective.contains(path) {
            effective
        } else {
            draft
        };
        self.preserve_selected_path(path, previous);
        if !self.contains(path) {
            self.insert(path.to_string());
        }
    }

    pub fn insert_with_identity(&mut self, path: String, identity: String) -> bool {
        use std::collections::btree_map::Entry;
        if let Entry::Vacant(entry) = self.entries.entry(path) {
            entry.insert(Some(identity));
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, path: &str) -> bool {
        self.entries.remove(path).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn identities(&self) -> BTreeMap<String, String> {
        self.entries
            .iter()
            .filter_map(|(path, identity)| {
                identity
                    .as_ref()
                    .map(|identity| (path.clone(), identity.clone()))
            })
            .collect()
    }

    /// Reconcile with current discovery without adopting new identities: drop
    /// a vanished path or one whose captured identity changed.
    fn sync(&mut self, detected: &BTreeMap<String, Option<String>>) {
        self.entries.retain(|path, recorded| {
            let Some(current) = detected.get(path) else {
                return false;
            };
            current
                .as_ref()
                .is_none_or(|current| recorded.as_ref().is_none_or(|recorded| recorded == current))
        });
    }

    fn retain_current_identities(&mut self, detected: &BTreeMap<String, Option<String>>) {
        self.entries.retain(|path, identity| {
            identity
                .as_ref()
                .zip(detected.get(path).and_then(Option::as_ref))
                .is_some_and(|(recorded, current)| recorded == current)
        });
    }

    fn paths(&self) -> BTreeSet<String> {
        self.entries.keys().cloned().collect()
    }
}

impl PartialEq<BTreeSet<String>> for Checklist {
    fn eq(&self, other: &BTreeSet<String>) -> bool {
        self.paths() == *other
    }
}

impl FromIterator<String> for Checklist {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        Self {
            entries: iter.into_iter().map(|path| (path, None)).collect(),
        }
    }
}

/// The local owner's sharing session, as the core reports it.
///
/// `pairing` holds the pairing secrets exactly once, in memory only: they are
/// never written to the settings file, a log or a device. Checking is
/// session-only, so `pending_checked` starts empty on every launch.
pub struct SharingState {
    /// Whether this terminal owns a running session.
    pub active: bool,
    /// The owner-visible snapshot (no secrets), when active.
    pub snapshot: Option<SharingSnapshot>,
    /// The session's pairing information, including both secrets.
    pub pairing: Option<ShareSessionInfo>,
    /// Device paths checked for sharing. While a session is running this
    /// mirrors the core's authoritative checked list; before it starts it is
    /// the selection the host made in the settings.
    pub pending_checked: Checklist,
}

impl SharingState {
    pub fn new() -> Self {
        Self {
            active: false,
            snapshot: None,
            pairing: None,
            pending_checked: Checklist::new(),
        }
    }

    /// The checked paths, from the core when a session is running.
    pub fn checked(&self) -> BTreeSet<String> {
        match &self.snapshot {
            Some(snapshot) if self.active => snapshot
                .checked
                .iter()
                .map(|device| device.path.clone())
                .collect(),
            _ => self.pending_checked.paths(),
        }
    }

    /// Drop every secret and every trace of a session.
    pub fn clear_session(&mut self) {
        self.active = false;
        self.snapshot = None;
        self.pairing = None;
    }
}

impl Default for SharingState {
    fn default() -> Self {
        Self::new()
    }
}

/// One connected host. `0` is this machine's core; every remote link gets a
/// fresh number, so two hosts may both have a device with id 1 and a path like
/// `/dev/ttyUSB0` without colliding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(pub u64);

/// This machine's core.
pub const LOCAL_ENDPOINT: EndpointId = EndpointId(0);

/// One attached device: its endpoint and the core's device id on that host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionKey {
    pub endpoint: EndpointId,
    pub device_id: u64,
}

impl SessionKey {
    pub fn local(device_id: u64) -> Self {
        Self {
            endpoint: LOCAL_ENDPOINT,
            device_id,
        }
    }
}

/// Which fields a form shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormMode {
    /// Address, port and one copy-and-paste sharing key.
    Key,
    /// The older host fields, for connecting to a host that predates keys.
    Advanced,
}

/// One field of the remote-connect form, in drawing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectField {
    Address,
    Port,
    Key,
    Ca,
    Fingerprint,
    Credential,
}

impl ConnectField {
    /// The fields of the normal form: address, port and the human key.
    pub const NORMAL: [Self; 3] = [Self::Address, Self::Port, Self::Key];
    /// The fields of the older-host form.
    pub const ADVANCED: [Self; 5] = [
        Self::Address,
        Self::Port,
        Self::Ca,
        Self::Fingerprint,
        Self::Credential,
    ];

    pub fn fields(mode: FormMode) -> &'static [Self] {
        match mode {
            FormMode::Key => &Self::NORMAL,
            FormMode::Advanced => &Self::ADVANCED,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Address => "Host address",
            Self::Port => "Port",
            Self::Key => "Human sharing key",
            Self::Ca => "CA certificate (base64 DER)",
            Self::Fingerprint => "Host certificate SHA-256",
            Self::Credential => "Human pairing credential",
        }
    }

    fn text_mut(self, form: &mut ConnectForm) -> &mut String {
        match self {
            Self::Address => &mut form.address,
            Self::Port => &mut form.port,
            Self::Key => &mut form.key,
            Self::Ca => &mut form.ca,
            Self::Fingerprint => &mut form.fingerprint,
            Self::Credential => &mut form.credential,
        }
    }
}

/// The remote-connect form. Every value is memory-only: nothing here is
/// written to the settings file, logged, or sent to a device. The sharing key
/// is password-equivalent, so it is masked like a credential and never saved.
pub struct ConnectForm {
    /// The normal (address, port, key) or older-host (advanced) fields.
    pub mode: FormMode,
    /// The field that takes typed text.
    pub field: ConnectField,
    pub address: String,
    pub port: String,
    pub key: String,
    pub ca: String,
    pub fingerprint: String,
    pub credential: String,
    pub error: Option<String>,
    /// Set when a typed character or paste into the key field was invalid.
    /// Submission is refused until the key is started again, so a malformed
    /// paste can never be silently repaired into an accepted key.
    pub key_invalid: bool,
}

impl ConnectForm {
    pub fn new() -> Self {
        Self {
            mode: FormMode::Key,
            field: ConnectField::Address,
            address: String::new(),
            port: String::new(),
            key: String::new(),
            ca: String::new(),
            fingerprint: String::new(),
            credential: String::new(),
            error: None,
            key_invalid: false,
        }
    }

    /// What the form shows for one field. The key and credential are masked.
    pub fn value(&self, field: ConnectField) -> String {
        match field {
            ConnectField::Address => self.address.clone(),
            ConnectField::Port => self.port.clone(),
            ConnectField::Key => mask(&self.key),
            ConnectField::Ca => self.ca.clone(),
            ConnectField::Fingerprint => self.fingerprint.clone(),
            ConnectField::Credential => mask(&self.credential),
        }
    }

    /// An advanced form filled with the older host fields, for an older host.
    pub fn advanced() -> Self {
        Self {
            mode: FormMode::Advanced,
            ..Self::new()
        }
    }

    pub fn fields(&self) -> &'static [ConnectField] {
        ConnectField::fields(self.mode)
    }

    /// Switch between the sharing-key form and the older-host form. The
    /// address and port carry over; the other side's entries are kept in
    /// memory but never submitted while that mode is hidden.
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            FormMode::Key => FormMode::Advanced,
            FormMode::Advanced => FormMode::Key,
        };
        self.field = ConnectField::Address;
        self.error = None;
    }

    /// Type one character. A sharing key never silently drops one: a space or
    /// any other character a key cannot contain is reported instead.
    pub fn push(&mut self, ch: char) {
        let strict = self.field == ConnectField::Key;
        let result = type_into(self.field.text_mut(self), strict, ch);
        if strict && result.is_err() {
            self.key_invalid = true;
        }
        self.error = result.err().map(str::to_string);
    }

    /// Insert text as one action, as a terminal paste does.
    ///
    /// A sharing key is taken as pasted (surrounding whitespace trimmed) and
    /// either inserted whole or refused: a key is never silently repaired or
    /// truncated, so the strict parser sees exactly what the person pasted.
    /// Other fields keep their printable-only behaviour, but an oversized
    /// paste is refused rather than cut short.
    pub fn push_str(&mut self, text: &str) {
        let strict = self.field == ConnectField::Key;
        let was_empty = self.field.text_mut(self).is_empty();
        let result = paste_into(self.field.text_mut(self), strict, text);
        if strict {
            if result.is_err() {
                self.key_invalid = true;
            } else if was_empty {
                self.key_invalid = false;
            }
        }
        self.error = result.err().map(str::to_string);
    }

    pub fn backspace(&mut self) {
        let is_key = self.field == ConnectField::Key;
        let field = self.field.text_mut(self);
        field.pop();
        // Only clearing the key itself starts the key over; editing another
        // field must not hide a malformed key that was already rejected.
        if is_key && field.is_empty() {
            self.key_invalid = false;
        }
        self.error = None;
    }

    /// Clear the complete human sharing key without touching the address or
    /// port. The next submission is refused until a whole key is pasted.
    pub fn clear_key(&mut self) {
        self.key.clear();
        self.key_invalid = false;
        self.error = None;
    }

    pub fn next_field(&mut self, delta: i32) {
        let fields = self.fields();
        let index = fields
            .iter()
            .position(|field| *field == self.field)
            .unwrap_or(0);
        let last = fields.len() - 1;
        let next = (index as i32 + delta).clamp(0, last as i32) as usize;
        self.field = fields[next];
    }
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self::new()
    }
}

/// Insert one typed character into a field. For a sharing key, anything that
/// is not printable ASCII is an error instead of a silent omission.
fn type_into(field: &mut String, strict: bool, ch: char) -> Result<(), &'static str> {
    if strict && !ch.is_ascii_graphic() {
        return Err("a sharing key can contain only printable ASCII characters");
    }
    if !strict && !ch.is_ascii_graphic() {
        return Ok(());
    }
    if field.len() >= MAX_FORM_FIELD_BYTES {
        return Err("the field is full");
    }
    field.push(ch);
    Ok(())
}

/// Insert one whole pasted value into a field.
///
/// `strict` is used for sharing keys: surrounding whitespace is allowed (a
/// terminal paste often ends with a newline), but every remaining character
/// must be printable ASCII and the result must fit the field. Nothing is
/// dropped or truncated, so the key parser validates exactly what was pasted.
fn paste_into(field: &mut String, strict: bool, text: &str) -> Result<(), &'static str> {
    let trimmed = text.trim();
    if strict && !trimmed.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(
            "the pasted sharing key contains a space, line break or non-ASCII character; paste it again unchanged",
        );
    }
    let value: String = if strict {
        trimmed.to_string()
    } else {
        trimmed.chars().filter(|ch| ch.is_ascii_graphic()).collect()
    };
    if field.len() + value.len() > MAX_FORM_FIELD_BYTES {
        return Err("the pasted value is too long for this field");
    }
    field.push_str(&value);
    Ok(())
}

fn mask(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        "••••••••".to_string()
    }
}

/// Most one form field may hold. A sharing key is bounded well below this.
pub const MAX_FORM_FIELD_BYTES: usize = 4096;

/// One field of the MCP/AI pairing form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpField {
    Address,
    Port,
    Key,
}

impl McpField {
    pub const ALL: [Self; 3] = [Self::Address, Self::Port, Self::Key];

    pub fn name(self) -> &'static str {
        match self {
            Self::Address => "Host address",
            Self::Port => "Port",
            Self::Key => "MCP/AI sharing key",
        }
    }

    fn text_mut(self, form: &mut McpPairForm) -> &mut String {
        match self {
            Self::Address => &mut form.address,
            Self::Port => &mut form.port,
            Self::Key => &mut form.key,
        }
    }
}

/// The form that saves one endpoint's MCP/AI pairing to the private pairing
/// file. It never connects, requests control or enables sharing. The key stays
/// in memory only and is written to no other place.
pub struct McpPairForm {
    pub field: McpField,
    pub address: String,
    pub port: String,
    pub key: String,
    pub error: Option<String>,
    /// True once a person has been told the endpoint is already paired and
    /// asked to press Enter again; any edit cancels that confirmation.
    pub confirm_replace: bool,
    /// Set when a typed character or paste into the AI key was invalid; saving
    /// is refused until the key is started again.
    pub key_invalid: bool,
}

impl McpPairForm {
    pub fn new() -> Self {
        Self {
            field: McpField::Address,
            address: String::new(),
            port: String::new(),
            key: String::new(),
            error: None,
            confirm_replace: false,
            key_invalid: false,
        }
    }

    pub fn value(&self, field: McpField) -> String {
        match field {
            McpField::Address => self.address.clone(),
            McpField::Port => self.port.clone(),
            McpField::Key => mask(&self.key),
        }
    }

    /// Type one character, strict for the MCP/AI key: an invalid character is
    /// reported and blocks saving instead of being trimmed away.
    pub fn push(&mut self, ch: char) {
        let strict = self.field == McpField::Key;
        let result = type_into(self.field.text_mut(self), strict, ch);
        if strict && result.is_err() {
            self.key_invalid = true;
        }
        self.error = result.err().map(str::to_string);
        self.confirm_replace = false;
    }

    /// Insert text as one action, strict for the AI key: a key is inserted
    /// whole or refused, never silently repaired or truncated.
    pub fn push_str(&mut self, text: &str) {
        let strict = self.field == McpField::Key;
        let was_empty = self.field.text_mut(self).is_empty();
        let result = paste_into(self.field.text_mut(self), strict, text);
        if strict {
            if result.is_err() {
                self.key_invalid = true;
            } else if was_empty {
                self.key_invalid = false;
            }
        }
        self.error = result.err().map(str::to_string);
        self.confirm_replace = false;
    }

    pub fn backspace(&mut self) {
        let is_key = self.field == McpField::Key;
        let field = self.field.text_mut(self);
        field.pop();
        // Only clearing the key itself starts the key over.
        if is_key && field.is_empty() {
            self.key_invalid = false;
        }
        self.error = None;
        self.confirm_replace = false;
    }

    pub fn next_field(&mut self, delta: i32) {
        let index = McpField::ALL
            .iter()
            .position(|field| *field == self.field)
            .unwrap_or(0);
        let last = McpField::ALL.len() - 1;
        let next = (index as i32 + delta).clamp(0, last as i32) as usize;
        self.field = McpField::ALL[next];
    }
}

impl Default for McpPairForm {
    fn default() -> Self {
        Self::new()
    }
}

/// One sharing key the owner asked to copy to the terminal's clipboard.
///
/// `Debug` never prints the key: it is password-equivalent.
#[derive(Clone, PartialEq, Eq)]
pub struct ClipboardCopy {
    /// What was copied, for the footer notice.
    pub label: &'static str,
    /// The complete key string, unchanged.
    pub key: String,
}

impl std::fmt::Debug for ClipboardCopy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardCopy")
            .field("label", &self.label)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// One sidebar row: a host group heading or a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarRow {
    Group(EndpointId),
    Device(usize),
}

/// One row in the sidebar: a serial device the core or a remote host shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    /// The host this device belongs to.
    pub endpoint: EndpointId,
    pub path: String,
    pub description: Option<String>,
    /// Opaque device-node identity, when the core could capture it.
    pub identity: Option<String>,
    pub open: bool,
    pub device_id: Option<u64>,
    pub baud: Option<u32>,
    /// Short label of the AI control holder, if any.
    pub control: Option<String>,
    /// Stable core client id of the AI control holder.
    pub control_id: Option<u64>,
}

impl DeviceEntry {
    pub fn new(path: impl Into<String>) -> Self {
        Self::on(LOCAL_ENDPOINT, path)
    }

    /// A detected entry with a synthetic identity, for tests and local-only
    /// construction. Real discovery replaces it with the core's identity.
    pub fn on(endpoint: EndpointId, path: impl Into<String>) -> Self {
        let path = path.into();
        let identity = Some(format!("entry:{path}"));
        Self {
            endpoint,
            path,
            description: None,
            identity,
            open: false,
            device_id: None,
            baud: None,
            control: None,
            control_id: None,
        }
    }
}

/// A device this terminal is attached to.
pub struct Session {
    pub endpoint: EndpointId,
    pub device_id: u64,
    pub path: String,
    pub baud: u32,
    pub control: Option<String>,
    pub control_id: Option<u64>,
    pub screen: Screen,
    /// Same size as every screen; kept here to size new sessions.
    rows: usize,
    cols: usize,
}

/// Baud prompt shown when opening a closed device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaudPrompt {
    pub endpoint: EndpointId,
    pub path: String,
    pub input: String,
    pub error: Option<String>,
}

impl BaudPrompt {
    pub fn new(path: impl Into<String>) -> Self {
        Self::on(LOCAL_ENDPOINT, path)
    }

    pub fn on(endpoint: EndpointId, path: impl Into<String>) -> Self {
        Self {
            endpoint,
            path: path.into(),
            input: String::new(),
            error: None,
        }
    }

    /// Typed rate, or [`DEFAULT_BAUD`] when the field is left empty.
    pub fn value(&self) -> Result<u32, String> {
        let text = self.input.trim();
        if text.is_empty() {
            return Ok(DEFAULT_BAUD);
        }
        match text.parse::<u32>() {
            Ok(baud) if (1..=4_000_000).contains(&baud) => Ok(baud),
            Ok(_) => Err(format!("{text} is outside 1..4000000")),
            Err(_) => Err(format!("{text} is not a baud rate")),
        }
    }
}

/// Everything the renderer needs.
///
/// The sidebar's highlighted row and the device shown in the right pane are
/// deliberately separate: moving or clicking the highlight never changes where
/// typed device input goes.
pub struct AppState {
    pub devices: Vec<DeviceEntry>,
    /// The highlighted sidebar row. It stays on a device while the Settings
    /// button has the focus, so discovery cannot turn the button into a device.
    pub selected: usize,
    /// Which pane receives keys.
    pub focus: Focus,
    /// What the right pane shows.
    pub view: View,
    pub sessions: BTreeMap<SessionKey, Session>,
    /// The session shown in the right pane, if any.
    shown: Option<SessionKey>,
    pub notice: Option<Notice>,
    /// Serious warnings not yet shown, oldest first. The visible warning is in
    /// `notice`; these appear one at a time as each is dismissed.
    queued_warnings: VecDeque<Notice>,
    /// When the visible ordinary notice expires; `None` for a warning.
    notice_expires: Option<Instant>,
    /// An ordinary notice that arrived while a warning was visible. It is
    /// shown after the warning is dismissed, if it has not expired by then.
    deferred_notice: Option<(Notice, Instant)>,
    pub prompt: Option<BaudPrompt>,
    pub quit: bool,
    /// The sidebar button that has the highlight, when no device row does.
    pub selected_button: Option<SidebarButton>,
    /// Whether the sharing address or port row is taking typed text.
    pub settings_editing: bool,
    /// The port as typed, so a half-entered value survives until Save.
    pub port_text: String,
    /// Selected row of the settings view.
    pub settings_row: usize,
    /// First line of the pairing information the right pane shows.
    pub pairing_scroll: usize,
    /// The human's preferences in effect for this launch.
    pub settings: TerminalSettings,
    /// The settings being edited; only Ctrl+S copies them into `settings` and
    /// saves them. Escape throws them away.
    pub draft: TerminalSettings,
    /// The device checkboxes in the settings draft. Session-only.
    pub draft_checked: Checklist,
    /// This terminal's sharing session (owner only).
    pub sharing: SharingState,
    /// Labels for connected remote hosts, keyed by endpoint. The local
    /// endpoint is always present in the sidebar's grouping.
    pub remote_labels: BTreeMap<EndpointId, String>,
    /// The remote-connect form while it is open.
    pub connect: Option<ConnectForm>,
    /// The MCP/AI pairing form while it is open.
    pub mcp_pair: Option<McpPairForm>,
    /// Whether the Share page lists the older host fields as well.
    pub pairing_advanced: bool,
    /// Selected row of the Share page.
    pub share_row: usize,
    /// Whether the Share page's address or port row is taking typed text.
    pub share_editing: bool,
    /// In-form validation or action failure on the Share page. It is not a
    /// footer notification and is never hidden by one.
    pub share_error: Option<String>,
    /// A Share-page action that needs one more explicit activation.
    pub share_confirm: Option<ShareConfirm>,
    /// A key the owner asked to copy, as one unchanged string. Taken by the
    /// event loop, which asks the terminal to put it on the clipboard with
    /// OSC 52; nothing here touches the settings file or a device.
    pub clipboard_copy: Option<ClipboardCopy>,
    /// A key asked for as one raw, soft-wrapped line, for terminals where the
    /// clipboard escape is unavailable. The event loop shows it until a key is
    /// pressed, then clears it.
    pub show_key: Option<ClipboardCopy>,
    rows: usize,
    cols: usize,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

impl AppState {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            devices: Vec::new(),
            selected: 0,
            selected_button: None,
            focus: Focus::Sidebar,
            view: View::Stream,
            sessions: BTreeMap::new(),
            shown: None,
            notice: None,
            queued_warnings: VecDeque::new(),
            notice_expires: None,
            deferred_notice: None,
            prompt: None,
            quit: false,
            settings_editing: false,
            port_text: String::new(),
            settings_row: 0,
            pairing_scroll: 0,
            settings: TerminalSettings::default(),
            draft: TerminalSettings::default(),
            draft_checked: Checklist::new(),
            sharing: SharingState::new(),
            remote_labels: BTreeMap::new(),
            connect: None,
            mcp_pair: None,
            pairing_advanced: false,
            share_row: 0,
            share_editing: false,
            share_error: None,
            share_confirm: None,
            clipboard_copy: None,
            show_key: None,
            rows: rows.max(1),
            cols: cols.max(1),
        }
    }

    /// Show an ordinary notice now. It expires after [`NOTICE_TTL`], unless a
    /// serious warning is visible: then it waits behind the warning instead of
    /// replacing it.
    pub fn set_notice(&mut self, notice: Notice) {
        self.set_notice_at(notice, Instant::now());
    }

    pub fn set_notice_at(&mut self, notice: Notice, now: Instant) {
        if notice.sticky {
            if self.notice.as_ref().is_some_and(|current| current.sticky) {
                self.queued_warnings.push_back(notice);
            } else {
                self.notice = Some(notice);
                self.notice_expires = None;
                self.focus_compact_warning();
            }
        } else if self.notice.as_ref().is_some_and(|current| current.sticky) {
            self.deferred_notice = Some((notice, now + NOTICE_TTL));
        } else {
            self.deferred_notice = None;
            self.notice = Some(notice);
            self.notice_expires = Some(now + NOTICE_TTL);
        }
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.set_notice(Notice::info(text));
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.set_notice(Notice::error(text));
    }

    pub fn warning(&mut self, text: impl Into<String>) {
        self.set_notice(Notice::warning(text));
    }

    /// On a sidebar too short to show a device row and the warning's Dismiss
    /// row separately, the label covers the device list. Move the keyboard
    /// highlight to the visible Dismiss control so Enter does what the screen
    /// says.
    fn focus_compact_warning(&mut self) {
        if self.selected_button.is_none()
            && crate::render::short_warning_shares_list(self.rows, self.cols)
            && self.sidebar_buttons().contains(&SidebarButton::Dismiss)
        {
            self.selected_button = Some(SidebarButton::Dismiss);
        }
    }

    /// Drop an ordinary notice, as a successful corrective action does. A
    /// serious warning is never cleared this way.
    pub fn clear_ordinary_notice(&mut self) {
        self.deferred_notice = None;
        if self.notice.as_ref().is_some_and(|current| !current.sticky) {
            self.notice = None;
            self.notice_expires = None;
        }
    }

    /// Expire an ordinary notice once its six seconds are up. Warnings stay.
    pub fn expire_notices(&mut self, now: Instant) {
        if let Some(expires) = self.notice_expires {
            if expires <= now && self.notice.as_ref().is_some_and(|current| !current.sticky) {
                self.notice = None;
                self.notice_expires = None;
            }
        }
        if self
            .deferred_notice
            .as_ref()
            .is_some_and(|(_, expires)| *expires <= now)
        {
            self.deferred_notice = None;
        }
        if self.notice.is_none() {
            if let Some((notice, expires)) = self.deferred_notice.take() {
                if expires > now {
                    self.notice = Some(notice);
                    self.notice_expires = Some(expires);
                }
            }
        }
    }

    /// Whether the visible notice is a warning that must be dismissed.
    pub fn has_warning(&self) -> bool {
        self.notice.as_ref().is_some_and(|notice| notice.sticky) || !self.queued_warnings.is_empty()
    }

    /// How many serious warnings remain, including the visible one.
    pub fn warning_count(&self) -> usize {
        usize::from(self.notice.as_ref().is_some_and(|notice| notice.sticky))
            + self.queued_warnings.len()
    }

    /// The visible warning's one-based position among the remaining warnings.
    pub fn warning_position(&self) -> Option<(usize, usize)> {
        if !self.notice.as_ref().is_some_and(|notice| notice.sticky) {
            return None;
        }
        let total = self.warning_count();
        Some((total - self.queued_warnings.len(), total))
    }

    /// Dismiss the visible serious warning, making the next one (or a waiting
    /// ordinary notice) visible. Returns whether there was one to dismiss.
    pub fn dismiss_warning(&mut self) -> bool {
        if !self.notice.as_ref().is_some_and(|notice| notice.sticky) {
            return false;
        }
        self.notice = self.queued_warnings.pop_front();
        self.notice_expires = None;
        if self.notice.is_none() {
            if let Some((notice, _)) = self.deferred_notice.take() {
                self.notice = Some(notice);
                self.notice_expires = Some(Instant::now() + NOTICE_TTL);
            }
        }
        true
    }

    /// What one settings row edits. The first four rows are fixed; the rest
    /// are one checkbox per detected device.
    pub fn settings_kind(&self, row: usize) -> Option<SettingsRow> {
        if let Some((kind, _)) = FIXED_SETTINGS.get(row) {
            return Some(*kind);
        }
        // Only this host's devices can be shared: another host's checklist is
        // never ours to edit, and equal paths must not stand in for a local
        // device.
        let local = self.local_device_indices();
        let index = *local.get(row.checked_sub(SETTINGS_FIXED_ROWS)?)?;
        Some(SettingsRow::Device(index))
    }

    /// The device indices that belong to this machine's core, in list order.
    fn local_device_indices(&self) -> Vec<usize> {
        self.devices
            .iter()
            .enumerate()
            .filter(|(_, device)| device.endpoint == LOCAL_ENDPOINT)
            .map(|(index, _)| index)
            .collect()
    }

    /// How many settings rows there are, including one per detected local
    /// device.
    pub fn settings_row_count(&self) -> usize {
        SETTINGS_FIXED_ROWS + self.local_device_indices().len()
    }

    /// The name of a fixed settings row. Device rows are labelled by path.
    pub fn settings_row_name(row: usize) -> Option<&'static str> {
        FIXED_SETTINGS.get(row).map(|(_, name)| *name)
    }

    /// The label the settings view draws for one row.
    pub fn settings_row_label(&self, row: usize) -> String {
        if let Some(name) = Self::settings_row_name(row) {
            return name.to_string();
        }
        match self.settings_kind(row) {
            Some(SettingsRow::Device(index)) => self
                .devices
                .get(index)
                .map(|device| device.path.clone())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The value of one settings row as the settings view shows it.
    pub fn draft_value(&self, row: usize) -> Option<String> {
        match self.settings_kind(row)? {
            SettingsRow::LocalEcho => Some(on_off(self.draft.local_echo)),
            SettingsRow::Scrollback => Some(on_off(self.draft.scrollback)),
            SettingsRow::ShareAddress => Some(self.draft.share_address.clone()),
            SettingsRow::SharePort => Some(self.port_text.clone()),
            SettingsRow::Device(index) => {
                let path = &self.devices.get(index)?.path;
                Some(if self.draft_checked.contains(path) {
                    "on".into()
                } else {
                    "off".into()
                })
            }
        }
    }

    /// Whether the checkbox of one device row is checked in the draft.
    pub fn draft_device_checked(&self, row: usize) -> Option<bool> {
        let SettingsRow::Device(index) = self.settings_kind(row)? else {
            return None;
        };
        let path = &self.devices.get(index)?.path;
        Some(self.draft_checked.contains(path))
    }

    /// Move the settings selection, staying on a real row.
    pub fn move_settings_row(&mut self, delta: i32) {
        let last = self.settings_row_count().saturating_sub(1);
        self.settings_row = (self.settings_row as i32 + delta).clamp(0, last as i32) as usize;
        self.settings_editing = false;
    }

    /// Flip the selected flag or device checkbox in the draft. Text rows are
    /// edited by typing, never toggled, and return `None`.
    pub fn toggle_draft(&mut self) -> Option<bool> {
        match self.settings_kind(self.settings_row)? {
            SettingsRow::LocalEcho => {
                self.draft.local_echo = !self.draft.local_echo;
                Some(self.draft.local_echo)
            }
            SettingsRow::Scrollback => {
                self.draft.scrollback = !self.draft.scrollback;
                Some(self.draft.scrollback)
            }
            SettingsRow::Device(index) => {
                let device = self.devices.get(index)?;
                let path = device.path.clone();
                let checked = if self.draft_checked.remove(&path) {
                    false
                } else {
                    // Without a captured identity the core could only match a
                    // path, so a replacement could inherit this check.
                    let Some(identity) = device.identity.clone() else {
                        self.set_notice(Notice::error(
                            "this device has no captured identity; refresh and try again",
                        ));
                        return Some(false);
                    };
                    self.draft_checked.insert_with_identity(path, identity);
                    true
                };
                Some(checked)
            }
            SettingsRow::ShareAddress | SettingsRow::SharePort => None,
        }
    }

    fn selected_settings_text_mut(&mut self) -> Option<(&mut String, usize)> {
        match self.settings_kind(self.settings_row)? {
            SettingsRow::ShareAddress => Some((&mut self.draft.share_address, 64)),
            SettingsRow::SharePort => Some((&mut self.port_text, 5)),
            _ => None,
        }
    }

    /// Start typing into the selected address or port row.
    pub fn begin_settings_edit(&mut self) -> bool {
        let Some((text, _)) = self.selected_settings_text_mut() else {
            return false;
        };
        // A text field starts empty, so a value is typed rather than appended.
        text.clear();
        self.settings_editing = true;
        true
    }

    /// Add one typed character to the selected text row.
    pub fn settings_edit_push(&mut self, ch: char) {
        if !self.settings_editing || !ch.is_ascii_graphic() {
            return;
        }
        if let Some((text, limit)) = self.selected_settings_text_mut() {
            if text.len() < limit {
                text.push(ch);
            }
        }
    }

    pub fn settings_edit_backspace(&mut self) {
        if self.settings_editing {
            if let Some((text, _)) = self.selected_settings_text_mut() {
                text.pop();
            }
        }
    }

    /// Stop editing the text row. The draft keeps what was typed.
    pub fn end_settings_edit(&mut self) {
        self.settings_editing = false;
    }

    /// Whether the settings are open with changes that Save or Cancel would
    /// still keep or throw away.
    pub fn has_unsaved_settings(&self) -> bool {
        self.view == View::Settings
            && (self.draft != self.settings
                || self.draft_checked != self.sharing.checked()
                || self.port_text != self.draft.share_port.to_string())
    }

    /// Open the settings in the right pane. The draft begins as a copy of what
    /// is in effect, so Escape can throw every change away. Re-opening the
    /// settings that are already open keeps the draft exactly as it is.
    pub fn begin_settings(&mut self) {
        // Opening the settings abandons a baud prompt: the settings keys would
        // otherwise reach the hidden prompt.
        self.prompt = None;
        self.focus = Focus::Right;
        if self.view == View::Settings {
            return;
        }
        self.draft = self.settings.clone();
        // The checklist starts from what the core currently shares, or from
        // the session-only selection made before sharing started.
        self.draft_checked = self.sharing.pending_checked.clone();
        self.reconcile_share_checks(&self.detected_local_identities());
        self.port_text = self.draft.share_port.to_string();
        self.settings_row = 0;
        self.settings_editing = false;
        self.view = View::Settings;
        // A serious warning is the human's to dismiss, not this page's to
        // erase. An ordinary notice may give way to the new view.
        self.clear_ordinary_notice();
    }

    /// Leave the settings without keeping the draft.
    pub fn cancel_settings(&mut self) {
        self.draft = self.settings.clone();
        self.draft_checked = self.sharing.pending_checked.clone();
        self.port_text = self.draft.share_port.to_string();
        self.settings_editing = false;
        self.leave_settings();
    }

    /// Keep the draft (and its device checks) as the settings in effect and
    /// leave the view. The app calls this only after the file was written and
    /// the core accepted the checklist changes.
    pub fn commit_settings(&mut self) {
        self.settings = self.draft.clone();
        self.port_text = self.settings.share_port.to_string();
        self.sharing.pending_checked = self.draft_checked.clone();
        self.settings_editing = false;
        self.apply_scrollback();
        self.leave_settings();
    }

    /// The rows the Share page draws, in order: the address and port, one
    /// checkbox per local device, then the explicit buttons.
    pub fn share_rows(&self) -> Vec<ShareRow> {
        let mut rows = vec![ShareRow::Address, ShareRow::Port];
        rows.extend(
            self.local_device_indices()
                .into_iter()
                .map(ShareRow::Device),
        );
        rows.push(ShareRow::StartStop);
        if self.has_unsaved_share() {
            rows.push(ShareRow::Save);
        }
        if self.has_unsaved_share() || self.share_confirm.is_some() {
            rows.push(ShareRow::Cancel);
        }
        if self.sharing.active {
            rows.push(ShareRow::CopyHuman);
            rows.push(ShareRow::CopyAi);
            rows.push(ShareRow::ShowHuman);
            rows.push(ShareRow::ShowAi);
            rows.push(ShareRow::Advanced);
        }
        rows
    }

    /// The Share row the selection is on, clamped to the rows that exist now.
    pub fn selected_share_row(&self) -> ShareRow {
        let rows = self.share_rows();
        rows.get(self.share_row.min(rows.len().saturating_sub(1)))
            .copied()
            .unwrap_or(ShareRow::Address)
    }

    pub fn move_share_row(&mut self, delta: i32) {
        let rows = self.share_rows();
        let last = rows.len().saturating_sub(1) as i32;
        self.share_row = (self.share_row as i32 + delta).clamp(0, last) as usize;
        self.share_editing = false;
    }

    /// Start typing into the selected Share address or port row.
    pub fn begin_share_edit(&mut self) -> bool {
        let (text, _) = match self.selected_share_row() {
            ShareRow::Address => (&mut self.draft.share_address, 64),
            ShareRow::Port => (&mut self.port_text, 5),
            _ => return false,
        };
        text.clear();
        self.share_editing = true;
        self.share_error = None;
        true
    }

    pub fn share_edit_push(&mut self, ch: char) {
        if !self.share_editing || !ch.is_ascii_graphic() {
            return;
        }
        if let Some((text, limit)) = match self.selected_share_row() {
            ShareRow::Address => Some((&mut self.draft.share_address, 64)),
            ShareRow::Port => Some((&mut self.port_text, 5)),
            _ => None,
        } {
            if text.len() < limit {
                text.push(ch);
            }
        }
    }

    /// Insert a bracketed paste into the selected Share address or port. The
    /// field was cleared when editing began, so the paste becomes its value;
    /// non-printable characters and an oversized value are ignored rather than
    /// silently truncated.
    pub fn share_edit_push_str(&mut self, text: &str) {
        if !self.share_editing {
            return;
        }
        let value: String = text
            .trim()
            .chars()
            .filter(|ch| ch.is_ascii_graphic())
            .collect();
        if let Some((field, limit)) = match self.selected_share_row() {
            ShareRow::Address => Some((&mut self.draft.share_address, 64)),
            ShareRow::Port => Some((&mut self.port_text, 5)),
            _ => None,
        } {
            if field.len() + value.len() <= limit {
                field.push_str(&value);
            }
        }
    }

    pub fn share_edit_backspace(&mut self) {
        if !self.share_editing {
            return;
        }
        if let Some(text) = match self.selected_share_row() {
            ShareRow::Address => Some(&mut self.draft.share_address),
            ShareRow::Port => Some(&mut self.port_text),
            _ => None,
        } {
            text.pop();
        }
    }

    pub fn end_share_edit(&mut self) {
        self.share_editing = false;
    }

    /// Toggle the selected device check in the Share draft. Text rows and
    /// buttons are not toggles and return `None`.
    pub fn toggle_share_draft(&mut self) -> Option<bool> {
        let ShareRow::Device(index) = self.selected_share_row() else {
            return None;
        };
        let device = self.devices.get(index)?;
        let path = device.path.clone();
        let checked = if self.draft_checked.remove(&path) {
            false
        } else {
            let Some(identity) = device.identity.clone() else {
                self.share_error =
                    Some("this device has no captured identity; refresh and try again".to_string());
                return Some(false);
            };
            self.draft_checked.insert_with_identity(path, identity);
            true
        };
        self.share_error = None;
        Some(checked)
    }

    /// Whether the Share draft differs from the settings and checked set in
    /// effect. Saving or cancelling makes it false.
    pub fn has_unsaved_share(&self) -> bool {
        self.draft != self.settings
            || self.port_text != self.draft.share_port.to_string()
            || self.draft_checked != self.sharing.checked()
    }

    /// Open the Share page in the right pane. Its draft starts from what is in
    /// effect; reopening it keeps any edits already made.
    pub fn begin_share(&mut self) {
        self.prompt = None;
        self.focus = Focus::Right;
        if self.view == View::Share {
            return;
        }
        self.draft = self.settings.clone();
        self.draft_checked = self.sharing.pending_checked.clone();
        self.reconcile_share_checks(&self.detected_local_identities());
        self.port_text = self.draft.share_port.to_string();
        self.share_row = 0;
        self.share_editing = false;
        self.share_error = None;
        self.share_confirm = None;
        self.view = View::Share;
    }

    /// Keep the Share selection on a row that still exists after saving,
    /// cancelling or starting changed which buttons are shown.
    fn clamp_share_row(&mut self) {
        let rows = self.share_rows().len();
        self.share_row = self.share_row.min(rows.saturating_sub(1));
    }

    /// Throw away the Share page's edits and keep the effective share. The
    /// page stays open so the human can see it was not applied.
    pub fn cancel_share_edits(&mut self) {
        self.draft = self.settings.clone();
        self.draft_checked = self.sharing.pending_checked.clone();
        self.port_text = self.draft.share_port.to_string();
        self.share_editing = false;
        self.share_error = None;
        self.share_confirm = None;
        self.clamp_share_row();
    }

    /// Show the Share page's effective values after a successful save.
    pub fn commit_share_draft(&mut self) {
        self.settings = self.draft.clone();
        self.port_text = self.settings.share_port.to_string();
        self.sharing.pending_checked = self.draft_checked.clone();
        self.share_editing = false;
        self.share_error = None;
        self.share_confirm = None;
        self.apply_scrollback();
        self.clamp_share_row();
    }

    /// Keep every open screen's scrollback in line with the settings in effect.
    /// Turning it off frees the kept rows and brings the view back to live.
    fn apply_scrollback(&mut self) {
        let capacity = scrollback_capacity(&self.settings);
        for session in self.sessions.values_mut() {
            session.screen.set_scrollback(capacity);
        }
    }

    /// Return to the stream of the device the right pane was showing. Save and
    /// Cancel both land here, on the live screen the human was looking at.
    fn leave_settings(&mut self) {
        // A baud prompt belongs to the sidebar; leaving the settings must not
        // hand the returned stream's keys back to it.
        self.prompt = None;
        self.view = View::Stream;
        self.focus = Focus::Right;
        if let Some(session) = self.shown_session_mut() {
            session.screen.scroll_to_bottom();
        }
    }

    /// The sidebar's rows. With a connected remote host, devices are grouped
    /// under a heading per endpoint, so identical paths on two hosts cannot be
    /// mistaken for one device.
    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        let grouped = !self.remote_labels.is_empty();
        let mut rows = Vec::new();
        let mut last: Option<EndpointId> = None;
        for (index, device) in self.devices.iter().enumerate() {
            if grouped && last != Some(device.endpoint) {
                rows.push(SidebarRow::Group(device.endpoint));
                last = Some(device.endpoint);
            }
            rows.push(SidebarRow::Device(index));
        }
        // A connected host with no approved devices still gets its heading,
        // so the sidebar never hides an endpoint that is still connected.
        rows.extend(
            self.empty_remote_endpoints()
                .into_iter()
                .map(SidebarRow::Group),
        );
        rows
    }

    /// Connected remote endpoints that currently have no device rows.
    pub fn empty_remote_endpoints(&self) -> Vec<EndpointId> {
        self.remote_labels
            .keys()
            .filter(|endpoint| {
                !self
                    .devices
                    .iter()
                    .any(|device| device.endpoint == **endpoint)
            })
            .copied()
            .collect()
    }

    /// The heading of one endpoint's device group.
    pub fn endpoint_label(&self, endpoint: EndpointId) -> String {
        if endpoint == LOCAL_ENDPOINT {
            "Local".to_string()
        } else {
            self.remote_labels
                .get(&endpoint)
                .cloned()
                .unwrap_or_else(|| format!("Remote {}", endpoint.0))
        }
    }

    /// A short stable discriminator for one endpoint, used beside device rows
    /// and in the header when paths alone cannot tell two hosts apart.
    pub fn endpoint_tag(&self, endpoint: EndpointId) -> String {
        if endpoint == LOCAL_ENDPOINT {
            "local".to_string()
        } else {
            format!("#{}", endpoint.0)
        }
    }

    /// Whether the sidebar's Settings button is highlighted.
    pub fn settings_button_selected(&self) -> bool {
        self.selected_button == Some(SidebarButton::Settings)
    }

    /// The sidebar button with the highlight, if any.
    pub fn selected_button(&self) -> Option<SidebarButton> {
        self.selected_button
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Resize every device screen to the new right-pane size. Each screen keeps
    /// its kept rows and scrolled position, so a resize never teleports a
    /// session's scrollback or drops another session.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        let buttons = self.sidebar_buttons();
        if let Some(button) = self.selected_button {
            if !buttons.contains(&button) {
                self.selected_button = Some(SidebarButton::Settings);
            }
        }
        if self.has_warning() {
            self.focus_compact_warning();
        }
        for session in self.sessions.values_mut() {
            session.screen.resize(self.rows, self.cols);
            session.rows = self.rows;
            session.cols = self.cols;
        }
    }

    /// The highlighted device row, or `None` while a sidebar button is.
    pub fn selected_device(&self) -> Option<&DeviceEntry> {
        if self.selected_button.is_some() {
            return None;
        }
        self.devices.get(self.selected)
    }

    /// The key of the session shown in the right pane, if any.
    pub fn shown_key(&self) -> Option<SessionKey> {
        self.shown
    }

    /// The device shown in the right pane, found by its session.
    pub fn shown_device(&self) -> Option<&DeviceEntry> {
        let key = self.shown?;
        let session = self.sessions.get(&key)?;
        self.devices.iter().find(|device| {
            device.endpoint == key.endpoint
                && (device.device_id == Some(key.device_id) || device.path == session.path)
        })
    }

    /// The session shown in the right pane.
    pub fn shown_session(&self) -> Option<&Session> {
        self.shown.and_then(|key| self.sessions.get(&key))
    }

    pub fn shown_session_mut(&mut self) -> Option<&mut Session> {
        let key = self.shown?;
        self.sessions.get_mut(&key)
    }

    /// The session of the highlighted device row, if it is attached.
    pub fn highlighted_session(&self) -> Option<&Session> {
        let device = self.selected_device()?;
        self.sessions
            .values()
            .find(|session| session.endpoint == device.endpoint && session.path == device.path)
    }

    /// Show a session in the right pane without changing the highlight.
    pub fn show_device(&mut self, key: SessionKey) -> bool {
        if !self.sessions.contains_key(&key) {
            return false;
        }
        self.shown = Some(key);
        true
    }

    /// Show one local session, for callers that only use this core.
    pub fn show_local_device(&mut self, device_id: u64) -> bool {
        self.show_device(SessionKey::local(device_id))
    }

    /// The buttons the sidebar can draw at its current height, in drawing
    /// order. The navigation uses the same list as the renderer.
    pub fn sidebar_buttons(&self) -> Vec<SidebarButton> {
        crate::render::available_sidebar_buttons(self.rows, self.cols, self.has_warning())
    }

    /// Select one sidebar row. The shown stream is untouched: choosing a row
    /// never redirects typed device input.
    pub fn select_index(&mut self, index: usize) {
        if index >= self.devices.len() {
            let buttons = self.sidebar_buttons();
            self.selected_button = buttons
                .get(index - self.devices.len())
                .copied()
                .or_else(|| buttons.last().copied());
            return;
        }
        self.selected_button = None;
        self.selected = index;
    }

    /// Move the sidebar selection over the devices and the visible buttons.
    pub fn select_delta(&mut self, delta: i32) {
        let buttons = self.sidebar_buttons();
        let count = self.devices.len() + buttons.len();
        if count == 0 {
            return;
        }
        let last = count - 1;
        let current = match self.selected_button {
            Some(button) => buttons
                .iter()
                .position(|candidate| *candidate == button)
                .map(|position| self.devices.len() + position)
                .unwrap_or_else(|| self.devices.len() + buttons.len() - 1),
            None => self.selected,
        };
        let next = current as i32 + delta;
        self.select_index(next.clamp(0, last as i32) as usize);
    }

    /// Point the sidebar highlight at one of its visible buttons. An absent
    /// button (a warning's Dismiss entry on a very short sidebar) is ignored.
    pub fn select_sidebar_button(&mut self, button: SidebarButton) {
        if self.sidebar_buttons().contains(&button) {
            self.selected_button = Some(button);
        }
    }

    /// Point the sidebar highlight at the Settings button. The device row
    /// stays where it was, so the button is not a row that discovery can
    /// renumber into a device.
    pub fn select_settings_button(&mut self) {
        self.select_sidebar_button(SidebarButton::Settings);
    }

    pub fn select_path(&mut self, path: &str) -> bool {
        self.devices
            .iter()
            .position(|device| device.path == path)
            .map(|index| self.select_index(index))
            .is_some()
    }

    pub fn select_endpoint_path(&mut self, endpoint: EndpointId, path: &str) -> bool {
        self.devices
            .iter()
            .position(|device| device.endpoint == endpoint && device.path == path)
            .map(|index| self.select_index(index))
            .is_some()
    }

    /// Replace this machine's discovered device list.
    pub fn set_devices(&mut self, devices: Vec<DeviceEntry>) {
        self.set_endpoint_devices(LOCAL_ENDPOINT, devices);
    }

    /// Replace one endpoint's discovered device list, keeping the highlight
    /// and any sessions that host did not enumerate. Other endpoints' rows are
    /// untouched, so identical paths on two hosts stay separate.
    pub fn set_endpoint_devices(&mut self, endpoint: EndpointId, mut devices: Vec<DeviceEntry>) {
        let selected = self
            .selected_device()
            .map(|device| (device.endpoint, device.path.clone()));
        // The Share page's controls are positional rows too. Preserve the
        // selected device by endpoint and path, so discovery removing an
        // earlier row cannot turn a checkbox activation into Start/Stop.
        let share_selection = (self.view == View::Share).then(|| {
            let row = self.selected_share_row();
            let device = match row {
                ShareRow::Device(index) => self
                    .devices
                    .get(index)
                    .map(|device| (device.endpoint, device.path.clone())),
                _ => None,
            };
            (device, row)
        });
        for device in &mut devices {
            device.endpoint = endpoint;
        }
        // A remote host's list is authoritative: a session whose device it no
        // longer lists was revoked or unplugged, so drop it rather than
        // resurrect the row from the local session cache.
        if endpoint != LOCAL_ENDPOINT {
            let missing: Vec<(SessionKey, String)> = self
                .sessions
                .iter()
                .filter(|(_, session)| {
                    session.endpoint == endpoint
                        && devices
                            .iter()
                            .all(|device| device.device_id != Some(session.device_id))
                })
                .map(|(key, session)| (*key, session.path.clone()))
                .collect();
            if !missing.is_empty() {
                let paths: Vec<String> = missing.iter().map(|(_, path)| path.clone()).collect();
                for (key, _) in missing {
                    self.remove_session(key);
                }
                self.set_notice(Notice::error(format!(
                    "no longer shared by the remote host: {}",
                    paths.join(", ")
                )));
            }
        }
        for session in self.sessions.values() {
            if session.endpoint == endpoint
                && devices.iter().all(|device| device.path != session.path)
            {
                devices.push(DeviceEntry {
                    endpoint,
                    path: session.path.clone(),
                    description: None,
                    identity: None,
                    open: true,
                    device_id: Some(session.device_id),
                    baud: Some(session.baud),
                    control: session.control.clone(),
                    control_id: session.control_id,
                });
            }
        }
        // The host's device list is authoritative for open state and the AI
        // control holder; reconcile this endpoint's sessions by stable id.
        for session in self
            .sessions
            .values_mut()
            .filter(|session| session.endpoint == endpoint)
        {
            if let Some(device) = devices
                .iter()
                .find(|device| device.device_id == Some(session.device_id))
            {
                session.control = device.control.clone();
                session.control_id = device.control_id;
                if let Some(baud) = device.baud {
                    session.baud = baud;
                }
            }
        }
        self.devices.retain(|device| device.endpoint != endpoint);
        self.devices.extend(devices);
        self.devices.sort_by(|left, right| {
            left.endpoint
                .cmp(&right.endpoint)
                .then_with(|| left.path.cmp(&right.path))
        });
        // Device rows feed the settings checklist; keep the cursor on a real
        // row when discovery removes one.
        let settings_rows = self.settings_row_count();
        if self.settings_row >= settings_rows {
            self.settings_row = settings_rows.saturating_sub(1);
            self.settings_editing = false;
        }
        if let Some((device, row)) = share_selection {
            let target = match device {
                Some((endpoint, path)) => self
                    .devices
                    .iter()
                    .position(|candidate| candidate.endpoint == endpoint && candidate.path == path)
                    .map(ShareRow::Device)
                    .or(Some(ShareRow::Address)),
                None => Some(row),
            };
            let rows = self.share_rows();
            self.share_row = target
                .and_then(|target| rows.iter().position(|candidate| *candidate == target))
                .unwrap_or(0);
        }
        // A button keeps the highlight; a device row keeps its device by
        // endpoint and path when the list was re-sorted or grew.
        if self.selected_button.is_some() {
            return;
        }
        let row = selected
            .and_then(|(endpoint, path)| {
                self.devices
                    .iter()
                    .position(|device| device.endpoint == endpoint && device.path == path)
            })
            .unwrap_or(0);
        self.select_index(row);
    }

    /// Add a freshly opened local session (tests and the local-only path),
    /// point the sidebar at it and show it. The screen starts empty and is fed
    /// only by live events, so output is never duplicated by a history
    /// snapshot racing the live subscription.
    pub fn add_session(
        &mut self,
        device_id: u64,
        path: impl Into<String>,
        baud: u32,
        control: Option<Holder>,
    ) {
        self.add_endpoint_session(LOCAL_ENDPOINT, device_id, path, baud, control);
    }

    /// Add a freshly opened session on one host, point the sidebar at it and
    /// show it. The screen starts empty and is fed only by live events.
    pub fn add_endpoint_session(
        &mut self,
        endpoint: EndpointId,
        device_id: u64,
        path: impl Into<String>,
        baud: u32,
        control: Option<Holder>,
    ) {
        let path = path.into();
        let key = SessionKey {
            endpoint,
            device_id,
        };
        let (control_label, control_id) = control
            .as_ref()
            .map(|holder| (Some(short_holder(holder)), Some(holder.client_id)))
            .unwrap_or((None, None));
        let session = Session {
            endpoint,
            device_id,
            path: path.clone(),
            baud,
            control: control_label.clone(),
            control_id,
            screen: Screen::with_scrollback(
                self.rows,
                self.cols,
                scrollback_capacity(&self.settings),
            ),
            rows: self.rows,
            cols: self.cols,
        };
        self.sessions.insert(key, session);
        if let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.endpoint == endpoint && device.path == path)
        {
            device.open = true;
            device.device_id = Some(device_id);
            device.baud = Some(baud);
            device.control = control_label;
            device.control_id = control_id;
        } else {
            self.devices.push(DeviceEntry {
                endpoint,
                path: path.clone(),
                description: None,
                identity: None,
                open: true,
                device_id: Some(device_id),
                baud: Some(baud),
                control: control_label,
                control_id,
            });
            self.devices.sort_by(|left, right| {
                left.endpoint
                    .cmp(&right.endpoint)
                    .then_with(|| left.path.cmp(&right.path))
            });
        }
        if let Some(index) = self
            .devices
            .iter()
            .position(|device| device.endpoint == endpoint && device.path == path)
        {
            self.select_index(index);
        }
        self.shown = Some(key);
    }

    /// Drop a session. A disconnect never closes the terminal and never drops
    /// the settings view: when the shown device goes away, another attached
    /// session takes the right pane, or the pane shows its empty hint.
    pub fn remove_session(&mut self, key: SessionKey) -> Option<Session> {
        let session = self.sessions.remove(&key)?;
        for device in &mut self.devices {
            if device.endpoint == key.endpoint
                && (device.device_id == Some(key.device_id) || device.path == session.path)
            {
                device.open = false;
                device.device_id = None;
                device.baud = None;
                device.control = None;
                device.control_id = None;
            }
        }
        if self.shown == Some(key) {
            self.shown = self
                .sessions
                .iter()
                .min_by(|(left_key, _), (right_key, _)| {
                    left_key.endpoint.cmp(&right_key.endpoint).then_with(|| {
                        self.sessions[left_key]
                            .path
                            .cmp(&self.sessions[right_key].path)
                    })
                })
                .map(|(key, _)| *key);
            match self.view {
                // The stream forwards keys to the device, so it always shows
                // the live screen of the session that takes over.
                View::Stream => {
                    if let Some(session) = self.shown.and_then(|key| self.sessions.get_mut(&key)) {
                        session.screen.scroll_to_bottom();
                    }
                    // A stream that suddenly belongs to another device must
                    // not swallow the next queued keys as writes: the human
                    // opens or joins it again before typing.
                    self.focus = Focus::Sidebar;
                }
                // Scrollback keys only navigate; a remaining session keeps its
                // own kept rows and scrolled position. With nothing left, the
                // view falls back to the empty stream.
                View::Scrollback if self.shown.is_none() => self.view = View::Stream,
                View::Scrollback | View::Settings | View::Share | View::Connect | View::McpPair => {
                }
            }
        }
        Some(session)
    }

    /// Remove a connected host and everything that belongs to it, keeping the
    /// sidebar highlight on a remaining row.
    pub fn remove_endpoint(&mut self, endpoint: EndpointId) {
        let selected = self
            .selected_device()
            .map(|device| (device.endpoint, device.path.clone()));
        self.remove_endpoint_sessions(endpoint);
        self.devices.retain(|device| device.endpoint != endpoint);
        self.remote_labels.remove(&endpoint);
        if self.selected_button.is_none() {
            let row = selected
                .and_then(|(endpoint, path)| {
                    self.devices
                        .iter()
                        .position(|device| device.endpoint == endpoint && device.path == path)
                })
                .unwrap_or_else(|| self.selected.min(self.devices.len().saturating_sub(1)));
            self.select_index(row);
        }
        let settings_rows = self.settings_row_count();
        if self.settings_row >= settings_rows {
            self.settings_row = settings_rows.saturating_sub(1);
            self.settings_editing = false;
        }
    }

    /// Drop every session of one endpoint (its host disconnected).
    pub fn remove_endpoint_sessions(&mut self, endpoint: EndpointId) -> Vec<SessionKey> {
        let keys: Vec<SessionKey> = self
            .sessions
            .keys()
            .filter(|key| key.endpoint == endpoint)
            .copied()
            .collect();
        for key in &keys {
            self.remove_session(*key);
        }
        keys
    }

    /// Every attached session, with its endpoint and that host's device id.
    pub fn all_sessions(&self) -> Vec<SessionKey> {
        self.sessions.keys().copied().collect()
    }

    /// Take the core's authoritative sharing snapshot. A device the core no
    /// longer lists (unplugged, replaced at the same path, or revoked) also
    /// loses its checkbox: sharing it again needs an explicit check.
    pub fn apply_sharing_snapshot(&mut self, snapshot: SharingSnapshot) -> Vec<String> {
        let previous = self.sharing.checked();
        let next: BTreeSet<String> = snapshot
            .checked
            .iter()
            .map(|device| device.path.clone())
            .collect();
        let lost = if snapshot.active {
            previous.difference(&next).cloned().collect()
        } else {
            Vec::new()
        };
        if snapshot.active {
            for path in previous.difference(&next) {
                self.draft_checked.remove(path);
            }
            let detected = self.detected_local_identities();
            let pending_before_sync = self.sharing.pending_checked.clone();
            let draft_before_sync = self.draft_checked.clone();
            self.sharing.pending_checked = Checklist::new();
            for device in &snapshot.checked {
                self.sharing.pending_checked.preserve_snapshot_selection(
                    &device.path,
                    &pending_before_sync,
                    &draft_before_sync,
                );
            }
            self.draft_checked.sync(&detected);
            for device in &snapshot.checked {
                self.draft_checked
                    .preserve_selected_path(&device.path, &draft_before_sync);
            }
        }
        // An inactive snapshot says only that sharing is off; the session-only
        // checked selection stays for this launch, as an explicit stop does.
        self.sharing.active = snapshot.active;
        self.sharing.snapshot = snapshot.active.then_some(snapshot);
        if !self.sharing.active {
            self.sharing.pairing = None;
            self.close_share_view();
        }
        lost
    }

    /// Leave the Share page after sharing ended without the human asking.
    /// The keys go back to the sidebar, so a later `s` only reopens the page.
    pub fn close_share_view(&mut self) {
        if self.view == View::Share {
            self.view = View::Stream;
            self.focus = Focus::Sidebar;
        }
    }

    /// Drop checked devices that are no longer detected. The core revokes
    /// their authorization; the checkbox must not survive a replacement.
    /// Reconcile both checklists against current identities in one operation.
    /// A vanished path or a known replacement loses its selection. A check
    /// without a captured identity stays unverified until explicitly selected again.
    pub fn reconcile_share_checks(&mut self, detected: &BTreeMap<String, Option<String>>) {
        let pending_before_sync = self.sharing.pending_checked.clone();
        let draft_before_sync = self.draft_checked.clone();
        self.sharing.pending_checked.sync(detected);
        self.draft_checked.sync(detected);
        if self.sharing.active {
            if let Some(snapshot) = &self.sharing.snapshot {
                for device in &snapshot.checked {
                    self.sharing.pending_checked.preserve_snapshot_selection(
                        &device.path,
                        &pending_before_sync,
                        &draft_before_sync,
                    );
                    self.draft_checked
                        .preserve_selected_path(&device.path, &draft_before_sync);
                }
            }
        }
    }

    /// Drop a check whose recorded identity no longer matches the device at
    /// that path. Called before granting or starting, so a replacement made
    /// between two refreshes is not authorized by stale UI state.
    pub fn drop_replaced_checks(&mut self) {
        let detected = self.detected_local_identities();
        self.reconcile_share_checks(&detected);
        self.sharing
            .pending_checked
            .retain_current_identities(&detected);
        self.draft_checked.retain_current_identities(&detected);
    }

    /// The local devices the core currently detects, mapped to their identity.
    pub fn detected_local_identities(&self) -> BTreeMap<String, Option<String>> {
        self.devices
            .iter()
            .filter(|device| device.endpoint == LOCAL_ENDPOINT)
            .map(|device| (device.path.clone(), device.identity.clone()))
            .collect()
    }

    /// Apply one local core event.
    pub fn apply_event(&mut self, event: &Event) {
        self.apply_endpoint_event(LOCAL_ENDPOINT, event);
    }

    /// Apply one event from a specific host. Session lookups are keyed by the
    /// endpoint, so two hosts with the same device id cannot cross streams.
    pub fn apply_endpoint_event(&mut self, endpoint: EndpointId, event: &Event) {
        match event {
            Event::DeviceOutput { device_id, .. } => {
                let key = SessionKey {
                    endpoint,
                    device_id: *device_id,
                };
                if let Some(session) = self.sessions.get_mut(&key) {
                    if let Some(Ok(bytes)) = event.bytes() {
                        session.screen.feed(&bytes);
                    }
                }
            }
            Event::Write {
                device_id, source, ..
            } => {
                if *source == Source::Ai {
                    let key = SessionKey {
                        endpoint,
                        device_id: *device_id,
                    };
                    if let Some(session) = self.sessions.get_mut(&key) {
                        if let Some(Ok(bytes)) = event.bytes() {
                            session.screen.feed_ai(&bytes);
                        }
                    }
                }
            }
            Event::DeviceClosed {
                device_id,
                path,
                reason,
            } => {
                let key = SessionKey {
                    endpoint,
                    device_id: *device_id,
                };
                self.remove_session(key);
                self.set_notice(Notice::error(format!("unplugged: {path} ({reason})")));
            }
            Event::ControlGranted { device_id, holder } => {
                self.set_control(endpoint, *device_id, Some(holder));
            }
            Event::ControlReleased {
                device_id,
                holder,
                reason,
            } => {
                // Match the stable client id: a release event may omit the
                // name the holder had when control was granted.
                let key = SessionKey {
                    endpoint,
                    device_id: *device_id,
                };
                let matches = self
                    .sessions
                    .get(&key)
                    .is_some_and(|session| session.control_id == Some(holder.client_id))
                    || self.devices.iter().any(|device| {
                        device.endpoint == endpoint
                            && device.device_id == Some(*device_id)
                            && device.control_id == Some(holder.client_id)
                    });
                if matches {
                    self.set_control(endpoint, *device_id, None);
                }
                self.set_notice(Notice::info(format!("AI control released ({reason:?})")));
            }
            Event::ControlDenied {
                device_id, holder, ..
            } => {
                self.set_control(endpoint, *device_id, Some(holder));
            }
            Event::TransportClosed { reason } => {
                self.set_notice(Notice::warning(format!("core connection closed: {reason}")));
            }
            Event::CoreShutdown { reason } => {
                self.set_notice(Notice::info(format!("core shutting down: {reason}")));
            }
            Event::ClientJoined { .. } | Event::ClientLeft { .. } => {}
        }
    }

    fn set_control(&mut self, endpoint: EndpointId, device_id: u64, holder: Option<&Holder>) {
        let label = holder.map(short_holder);
        let control_id = holder.map(|holder| holder.client_id);
        let key = SessionKey {
            endpoint,
            device_id,
        };
        if let Some(session) = self.sessions.get_mut(&key) {
            session.control = label.clone();
            session.control_id = control_id;
        }
        for device in &mut self.devices {
            if device.endpoint == endpoint && device.device_id == Some(device_id) {
                device.control = label.clone();
                device.control_id = control_id;
            }
        }
    }

    /// Start the baud prompt for the highlighted closed device.
    pub fn begin_baud_prompt(&mut self) -> bool {
        let Some(device) = self.selected_device() else {
            self.set_notice(Notice::error("no device highlighted"));
            return false;
        };
        if device.device_id.is_some() {
            return false;
        }
        let endpoint = device.endpoint;
        let path = device.path.clone();
        self.prompt = Some(BaudPrompt::on(endpoint, path));
        true
    }

    pub fn prompt_push(&mut self, ch: char) {
        if let Some(prompt) = &mut self.prompt {
            if ch.is_ascii_digit() && prompt.input.len() < 8 {
                prompt.input.push(ch);
                prompt.error = None;
            }
        }
    }

    pub fn prompt_backspace(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.input.pop();
            prompt.error = None;
        }
    }
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

/// How many rows a screen keeps for scrolling, from the settings in effect.
fn scrollback_capacity(settings: &TerminalSettings) -> usize {
    if settings.scrollback {
        SCROLLBACK_ROWS
    } else {
        0
    }
}

pub fn short_holder(holder: &Holder) -> String {
    match &holder.name {
        Some(name) => name.clone(),
        None => format!("client {}", holder.client_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices(paths: &[&str]) -> Vec<DeviceEntry> {
        paths.iter().map(|path| DeviceEntry::new(*path)).collect()
    }

    #[test]
    fn highlighting_a_row_does_not_change_the_shown_stream() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.add_session(7, "/dev/a", 115_200, None);
        state.select_index(1);

        assert_eq!(state.selected_device().unwrap().path, "/dev/b");
        assert_eq!(
            state.shown_session().map(|session| session.device_id),
            Some(7),
            "the stream still belongs to the device that was opened"
        );
    }

    #[test]
    fn a_disconnect_shows_another_attached_session_and_keeps_settings_open() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.add_session(8, "/dev/b", 115_200, None);
        state.begin_settings();
        state.draft.local_echo = false;

        state.apply_event(&Event::DeviceClosed {
            device_id: 8,
            path: "/dev/b".into(),
            reason: "removed".into(),
        });

        assert_eq!(
            state.shown_session().map(|session| session.device_id),
            Some(7),
            "the other attached session takes the right pane"
        );
        assert_eq!(state.view, View::Settings, "the settings stay open");
        assert!(!state.draft.local_echo, "the draft is kept");
        let notice = state.notice.as_ref().expect("the disconnect is reported");
        assert!(notice.text.contains("unplugged"), "{}", notice.text);
    }

    #[test]
    fn a_disconnect_in_the_stream_returns_the_keys_to_the_sidebar() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.add_session(8, "/dev/b", 115_200, None);
        state.show_local_device(8);
        state.view = View::Stream;
        state.focus = Focus::Right;

        state.remove_session(SessionKey::local(8));

        assert_eq!(state.shown_session().unwrap().device_id, 7);
        assert_eq!(
            state.focus,
            Focus::Sidebar,
            "the next keys are commands, not writes to the fallback device"
        );
    }

    #[test]
    fn a_disconnect_in_scrollback_leaves_navigation_on_the_remaining_session() {
        let mut state = AppState::new(6, 20);
        state.add_session(7, "/dev/a", 115_200, None);
        state.add_session(8, "/dev/b", 115_200, None);
        for index in 0..12 {
            let line = format!("line{index}\r\n");
            state.apply_event(&Event::device_output(7, line.as_bytes()));
        }
        // B is the shown session; its own screen stays where it was.
        state.show_local_device(8);
        state
            .sessions
            .get_mut(&SessionKey::local(8))
            .unwrap()
            .screen
            .scroll_back(2);
        state.view = View::Scrollback;
        state.focus = Focus::Right;

        state.apply_event(&Event::DeviceClosed {
            device_id: 8,
            path: "/dev/b".into(),
            reason: "removed".into(),
        });

        assert_eq!(
            state.view,
            View::Scrollback,
            "the arrow keys still navigate, not write to the fallback"
        );
        assert_eq!(state.shown_session().unwrap().device_id, 7);
    }

    #[test]
    fn the_last_disconnect_leaves_the_stream_view_empty() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.view = View::Scrollback;

        state.remove_session(SessionKey::local(7));

        assert!(state.shown_session().is_none());
        assert_eq!(
            state.view,
            View::Stream,
            "the scrollback has nothing to show"
        );
    }

    #[test]
    fn restoring_the_highlight_by_path_survives_discovery_changes() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(1);
        state.set_devices(devices(&["/dev/0", "/dev/a", "/dev/b"]));
        assert_eq!(
            state.selected_device().map(|device| device.path.as_str()),
            Some("/dev/b"),
            "the highlight is restored by path, not by row"
        );
    }

    #[test]
    fn button_focus_survives_the_device_list_growing() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(0);
        state.select_settings_button();
        assert!(state.settings_button_selected());

        state.set_devices(devices(&["/dev/0", "/dev/a", "/dev/b"]));
        assert!(
            state.settings_button_selected(),
            "the button keeps the highlight"
        );
        assert_eq!(state.selected_device(), None);
    }

    #[test]
    fn save_and_cancel_return_to_the_shown_stream_not_the_highlight() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(1);
        state.begin_settings();
        state.draft.local_echo = false;

        state.cancel_settings();
        assert_eq!(state.view, View::Stream);
        assert_eq!(state.focus, Focus::Right);
        assert_eq!(
            state.shown_session().map(|session| session.device_id),
            Some(7),
            "the shown device did not follow the highlight"
        );
        assert_eq!(state.selected_device().unwrap().path, "/dev/b");

        state.begin_settings();
        state.draft.local_echo = false;
        state.commit_settings();
        assert_eq!(state.view, View::Stream);
        assert_eq!(state.shown_session().unwrap().device_id, 7);
    }

    #[test]
    fn reopening_the_open_settings_keeps_the_draft() {
        let mut state = AppState::new(10, 80);
        state.begin_settings();
        state.draft.local_echo = false;
        state.begin_settings();
        assert!(!state.draft.local_echo, "the unsaved draft is kept");
        assert_eq!(state.view, View::Settings);
    }

    #[test]
    fn unsaved_settings_are_only_reported_while_the_draft_differs() {
        let mut state = AppState::new(10, 80);
        assert!(!state.has_unsaved_settings());
        state.begin_settings();
        assert!(!state.has_unsaved_settings(), "a fresh draft matches");
        state.draft.local_echo = false;
        assert!(state.has_unsaved_settings());
        state.cancel_settings();
        assert!(!state.has_unsaved_settings());
    }

    fn session_state(rows: usize, scrollback: bool) -> AppState {
        let mut state = AppState::new(rows, 20);
        state.settings.scrollback = scrollback;
        state.draft = state.settings.clone();
        state.add_session(7, "/dev/ttyUSB0", 115_200, None);
        state
    }

    fn write_lines(state: &mut AppState, first: usize, count: usize) {
        for index in first..first + count {
            let line = format!("line{index}\r\n");
            state.apply_event(&Event::device_output(7, line.as_bytes()));
        }
    }

    #[test]
    fn a_session_keeps_scrolled_off_output_and_scrolls_back_through_it() {
        let mut state = session_state(4, true);
        write_lines(&mut state, 0, 8);
        let session = state.sessions.get(&SessionKey::local(7)).expect("session");
        assert_eq!(session.screen.history_len(), 5, "five lines scrolled off");
        assert_eq!(session.screen.scroll(), 0, "the live view is the start");

        let session = state.sessions.get_mut(&SessionKey::local(7)).unwrap();
        session.screen.scroll_back(4);
        assert_eq!(session.screen.scroll(), 4);
        let row: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();
        assert_eq!(row.trim_end(), "line1");

        session.screen.scroll_to_top();
        assert_eq!(session.screen.scroll(), session.screen.history_len());
        let row: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();
        assert_eq!(row.trim_end(), "line0");

        session.screen.scroll_to_bottom();
        assert_eq!(session.screen.scroll(), 0);
        assert!(!session.screen.is_scrolled_back());
    }

    #[test]
    fn scrolling_back_while_output_arrives_keeps_the_same_text_in_view() {
        let mut state = session_state(4, true);
        write_lines(&mut state, 0, 8);
        let session = state.sessions.get_mut(&SessionKey::local(7)).unwrap();
        session.screen.scroll_back(3);
        let before: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();

        write_lines(&mut state, 8, 2);
        let session = state.sessions.get(&SessionKey::local(7)).unwrap();
        assert!(session.screen.scroll() > 3, "the view stayed on its text");
        let after: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();
        assert_eq!(after, before);
    }

    #[test]
    fn without_the_scrollback_setting_nothing_is_kept_or_scrolled() {
        let mut state = session_state(4, false);
        write_lines(&mut state, 0, 12);
        let session = state.sessions.get_mut(&SessionKey::local(7)).unwrap();
        assert_eq!(session.screen.history_len(), 0);
        session.screen.scroll_back(5);
        assert_eq!(session.screen.scroll(), 0, "nothing to scroll to");
    }

    #[test]
    fn saving_scrollback_off_clears_every_sessions_history() {
        let mut state = session_state(4, true);
        write_lines(&mut state, 0, 12);
        assert!(
            state
                .sessions
                .get(&SessionKey::local(7))
                .unwrap()
                .screen
                .history_len()
                > 0
        );

        state.begin_settings();
        state.draft.scrollback = false;
        state.commit_settings();
        assert!(!state.settings.scrollback);
        let session = state.sessions.get(&SessionKey::local(7)).unwrap();
        assert_eq!(session.screen.history_len(), 0, "the rows are freed");
        assert_eq!(session.screen.scroll(), 0, "the view is back on live");
    }

    #[test]
    fn turning_scrollback_on_keeps_new_rows_from_then_on() {
        let mut state = session_state(4, false);
        write_lines(&mut state, 0, 8);
        state.begin_settings();
        state.draft.scrollback = true;
        state.commit_settings();
        write_lines(&mut state, 8, 8);
        assert!(
            state
                .sessions
                .get(&SessionKey::local(7))
                .unwrap()
                .screen
                .history_len()
                > 0
        );
    }

    #[test]
    fn resizing_keeps_every_sessions_scroll_position() {
        let mut state = session_state(6, true);
        write_lines(&mut state, 0, 12);
        state
            .sessions
            .get_mut(&SessionKey::local(7))
            .unwrap()
            .screen
            .scroll_back(4);
        let scroll = state
            .sessions
            .get(&SessionKey::local(7))
            .unwrap()
            .screen
            .scroll();
        assert!(scroll > 0);

        state.resize(4, 30);
        assert_eq!(
            state
                .sessions
                .get(&SessionKey::local(7))
                .unwrap()
                .screen
                .scroll(),
            scroll,
            "a resize keeps the scrolled position"
        );
        state.resize(8, 50);
        assert_eq!(
            state
                .sessions
                .get(&SessionKey::local(7))
                .unwrap()
                .screen
                .scroll(),
            scroll
        );
    }
}

#[cfg(test)]
mod sharing_tests {
    use super::*;
    use serial_nexus_protocol::{SharedDevice, SharingSnapshot};

    fn state_with_devices() -> AppState {
        let mut state = AppState::new(10, 80);
        state.set_devices(vec![
            DeviceEntry::new("/dev/ttyUSB0"),
            DeviceEntry::new("/dev/ttyUSB1"),
        ]);
        state
    }

    fn snapshot(checked: &[&str]) -> SharingSnapshot {
        SharingSnapshot {
            active: true,
            session_id: Some("session".into()),
            bind: Some("127.0.0.1:7331".parse().unwrap()),
            checked: checked
                .iter()
                .map(|path| SharedDevice {
                    path: (*path).into(),
                    identity: "dev=1:2 ino=3".into(),
                })
                .collect(),
            remote_clients: 0,
            pending_connections: 0,
        }
    }

    #[test]
    fn inactive_snapshot_keeps_session_checks_without_a_revocation_notice() {
        let mut state = state_with_devices();
        state
            .sharing
            .pending_checked
            .insert_with_identity("/dev/ttyUSB0".into(), "stable-node".into());
        state.sharing.active = true;
        state.sharing.snapshot = Some(snapshot(&["/dev/ttyUSB0"]));
        let mut stopped = snapshot(&[]);
        stopped.active = false;
        stopped.session_id = None;
        stopped.bind = None;

        let lost = state.apply_sharing_snapshot(stopped);

        assert!(lost.is_empty());
        assert!(state.sharing.pending_checked.contains("/dev/ttyUSB0"));
        assert!(!state.sharing.active);
    }

    #[test]
    fn an_active_snapshot_keeps_the_discovery_identity_of_a_newly_checked_device() {
        let mut state = state_with_devices();
        state.devices[0].identity = Some("snx-1".into());
        state.devices[1].identity = Some("snx-2".into());
        state.sharing.active = true;
        state.sharing.snapshot = Some(snapshot(&["/dev/ttyUSB0"]));
        state
            .sharing
            .pending_checked
            .insert_with_identity("/dev/ttyUSB0".into(), "snx-1".into());
        state
            .draft_checked
            .insert_with_identity("/dev/ttyUSB0".into(), "snx-1".into());
        state
            .draft_checked
            .insert_with_identity("/dev/ttyUSB1".into(), "snx-2".into());

        state.apply_sharing_snapshot(snapshot(&["/dev/ttyUSB0", "/dev/ttyUSB1"]));

        assert_eq!(
            state
                .sharing
                .pending_checked
                .identities()
                .get("/dev/ttyUSB1"),
            Some(&"snx-2".to_string())
        );
    }

    #[test]
    fn an_active_snapshot_restores_pending_checks_from_the_selected_draft() {
        let mut state = state_with_devices();
        state.devices[0].identity = Some("snx-1".into());
        state.devices[1].identity = Some("snx-2".into());
        state.sharing.active = true;
        state.sharing.snapshot = Some(snapshot(&["/dev/ttyUSB0", "/dev/ttyUSB1"]));
        state
            .sharing
            .pending_checked
            .insert_with_identity("/dev/ttyUSB0".into(), "snx-1".into());
        state
            .draft_checked
            .insert_with_identity("/dev/ttyUSB1".into(), "snx-2".into());

        let detected = state.detected_local_identities();
        state.reconcile_share_checks(&detected);

        assert_eq!(
            state
                .sharing
                .pending_checked
                .identities()
                .get("/dev/ttyUSB1"),
            Some(&"snx-2".to_string())
        );
    }

    #[test]
    fn an_active_snapshot_preserves_a_selected_path_until_the_core_revokes_it() {
        let mut state = state_with_devices();
        let mut active = snapshot(&["/dev/ttyUSB0"]);
        let identity = active.checked[0].identity.clone();
        state.sharing.active = true;
        state.sharing.snapshot = Some(active.clone());
        state
            .sharing
            .pending_checked
            .insert_with_identity("/dev/ttyUSB0".into(), identity.clone());
        state
            .draft_checked
            .insert_with_identity("/dev/ttyUSB0".into(), identity);
        state.devices.retain(|device| device.path != "/dev/ttyUSB0");

        state.apply_sharing_snapshot(active.clone());

        assert!(state.sharing.pending_checked.contains("/dev/ttyUSB0"));
        assert!(state.draft_checked.contains("/dev/ttyUSB0"));
        active.checked.clear();
        state.apply_sharing_snapshot(active);
        assert!(!state.sharing.pending_checked.contains("/dev/ttyUSB0"));
        assert!(!state.draft_checked.contains("/dev/ttyUSB0"));
    }

    #[test]
    fn a_missing_check_identity_needs_explicit_reselection_after_discovery_recovers() {
        let mut checklist: Checklist = ["/dev/ttyUSB0".to_string()].into_iter().collect();
        let detected = BTreeMap::from([(
            "/dev/ttyUSB0".to_string(),
            Some("new-generation".to_string()),
        )]);

        checklist.sync(&detected);
        assert!(checklist.contains("/dev/ttyUSB0"));
        assert!(checklist.identities().is_empty());
        checklist.retain_current_identities(&detected);

        assert!(!checklist.contains("/dev/ttyUSB0"));
    }

    #[test]
    fn checklist_represents_missing_identity_explicitly_without_adopting_it_on_sync() {
        let mut checklist: Checklist = ["/dev/ttyUSB0".to_string()].into_iter().collect();
        assert!(checklist.contains("/dev/ttyUSB0"));
        assert!(checklist.identities().is_empty());

        checklist.sync(&BTreeMap::from([(
            "/dev/ttyUSB0".to_string(),
            Some("stable-node".to_string()),
        )]));
        assert!(checklist.identities().is_empty());
    }

    #[test]
    fn sidebar_navigation_matches_the_drawn_button_rows() {
        let mut state = state_with_devices();
        for rows in 2..8 {
            state.resize(rows, 80);
            let drawn = crate::render::sidebar(ratatui::layout::Rect::new(0, 0, 20, rows as u16))
                .expect("sidebar")
                .share
                .height
                > 0;
            assert_eq!(
                state.sidebar_buttons().len(),
                if drawn { 3 } else { 1 },
                "navigation and drawing agree at {rows} rows"
            );
        }
    }

    #[test]
    fn a_short_sidebar_navigates_only_the_buttons_it_draws() {
        let mut state = state_with_devices();
        state.resize(2, 80);
        assert_eq!(state.sidebar_buttons(), vec![SidebarButton::Settings]);
        state.select_index(1);
        state.select_delta(1);
        assert_eq!(
            state.selected_button(),
            Some(SidebarButton::Settings),
            "the hidden Share/Connect rows are not selectable"
        );
        state.select_delta(1);
        assert_eq!(state.selected_button(), Some(SidebarButton::Settings));
    }

    #[test]
    fn the_sidebar_moves_over_devices_then_share_connect_and_settings() {
        let mut state = state_with_devices();
        state.select_index(1);
        state.select_delta(1);
        assert_eq!(state.selected_button(), Some(SidebarButton::Share));
        assert!(
            state.selected_device().is_none(),
            "a button holds the highlight"
        );
        state.select_delta(1);
        assert_eq!(state.selected_button(), Some(SidebarButton::Connect));
        state.select_delta(1);
        assert_eq!(state.selected_button(), Some(SidebarButton::Settings));
        state.select_delta(1);
        assert_eq!(
            state.selected_button(),
            Some(SidebarButton::Settings),
            "the last row is the last button"
        );
        state.select_delta(-10);
        assert_eq!(state.selected_button(), None);
        assert_eq!(state.selected_device().unwrap().path, "/dev/ttyUSB0");
    }

    #[test]
    fn the_checklist_is_session_only_and_a_snapshot_revokes_a_replaced_path() {
        let mut state = state_with_devices();
        state.begin_settings();
        state.settings_row = SETTINGS_FIXED_ROWS;
        assert_eq!(state.toggle_draft(), Some(true));
        assert!(state.has_unsaved_settings());
        state.cancel_settings();
        assert!(state.draft_checked.is_empty());

        state.begin_settings();
        state.settings_row = SETTINGS_FIXED_ROWS;
        assert_eq!(state.toggle_draft(), Some(true));
        state.commit_settings();
        assert!(state.sharing.pending_checked.contains("/dev/ttyUSB0"));

        // A running session's snapshot is authoritative: a path it no longer
        // lists loses its checkbox, and the draft follows.
        state.sharing.active = true;
        state.sharing.snapshot = Some(snapshot(&["/dev/ttyUSB0"]));
        state.begin_settings();
        assert_eq!(state.draft_device_checked(SETTINGS_FIXED_ROWS), Some(true));
        state.apply_sharing_snapshot(snapshot(&[]));
        assert_eq!(
            state.draft_device_checked(SETTINGS_FIXED_ROWS),
            Some(false),
            "a replacement at the path needs an explicit recheck"
        );
        assert!(state.sharing.pending_checked.is_empty());
    }

    #[test]
    fn the_host_checklist_lists_only_this_machines_devices() {
        let mut state = AppState::new(10, 80);
        let mut remote = DeviceEntry::on(EndpointId(7), "/dev/ttyREMOTE0");
        remote.identity = Some("remote-id".into());
        state.set_endpoint_devices(LOCAL_ENDPOINT, vec![DeviceEntry::new("/dev/ttyLOCAL0")]);
        state.set_endpoint_devices(EndpointId(7), vec![remote]);
        assert_eq!(state.settings_row_count(), SETTINGS_FIXED_ROWS + 1);
        assert_eq!(
            state.settings_kind(SETTINGS_FIXED_ROWS),
            Some(SettingsRow::Device(0))
        );
        assert_eq!(state.settings_kind(SETTINGS_FIXED_ROWS + 1), None);
        state.begin_settings();
        state.settings_row = SETTINGS_FIXED_ROWS;
        assert_eq!(state.toggle_draft(), Some(true));
        assert!(state.draft_checked.contains("/dev/ttyLOCAL0"));
        assert!(!state.draft_checked.contains("/dev/ttyREMOTE0"));
    }

    #[test]
    fn a_replacement_identity_loses_its_check_before_sharing_starts() {
        let mut state = state_with_devices();
        let mut device = DeviceEntry::new("/dev/ttyUSB0");
        device.identity = Some("old-node".into());
        state.set_devices(vec![device, DeviceEntry::new("/dev/ttyUSB1")]);
        state.begin_settings();
        state.settings_row = SETTINGS_FIXED_ROWS;
        assert_eq!(state.toggle_draft(), Some(true));
        assert!(state.draft_checked.contains("/dev/ttyUSB0"));

        let detected: BTreeMap<String, Option<String>> = [
            ("/dev/ttyUSB0".to_string(), Some("new-node".to_string())),
            ("/dev/ttyUSB1".to_string(), Some("other-node".to_string())),
        ]
        .into_iter()
        .collect();
        state.reconcile_share_checks(&detected);
        assert!(
            !state.draft_checked.contains("/dev/ttyUSB0"),
            "a different node at the reused path needs a new check"
        );
    }

    #[test]
    fn sidebar_navigation_has_no_controls_the_layout_cannot_draw() {
        let mut state = AppState::new(10, 7);
        assert!(state.sidebar_buttons().is_empty());
        state.select_index(0);
        state.select_delta(1);
        assert_eq!(state.selected_button(), None);
    }

    #[test]
    fn an_unplugged_path_is_pruned_from_the_checks() {
        let mut state = state_with_devices();
        state
            .sharing
            .pending_checked
            .insert_with_identity("/dev/ttyUSB0".into(), "gone".into());
        state
            .draft_checked
            .insert_with_identity("/dev/ttyUSB1".into(), "id1".into());
        let detected: BTreeMap<String, Option<String>> =
            [("/dev/ttyUSB1".to_string(), Some("id1".to_string()))]
                .into_iter()
                .collect();
        state.reconcile_share_checks(&detected);
        assert!(!state.sharing.pending_checked.contains("/dev/ttyUSB0"));
        assert!(state.draft_checked.contains("/dev/ttyUSB1"));
    }

    #[test]
    fn text_rows_are_edited_by_typing_not_toggled() {
        let mut state = state_with_devices();
        state.begin_settings();
        state.settings_row = 2;
        assert_eq!(state.toggle_draft(), None, "the address is not a flag");
        assert!(state.begin_settings_edit());
        state.settings_edit_push('1');
        state.settings_edit_push('.');
        assert_eq!(state.draft.share_address, "1.");
        state.settings_edit_backspace();
        assert_eq!(state.draft.share_address, "1");
        state.end_settings_edit();

        state.settings_row = 3;
        assert_eq!(state.toggle_draft(), None);
        assert!(state.begin_settings_edit());
        state.settings_edit_push('9');
        assert_eq!(state.port_text, "9", "the port is typed as text");
        state.cancel_settings();
        assert_eq!(state.port_text, "7331");
    }

    #[test]
    fn settings_row_kinds_follow_the_detected_devices() {
        let state = state_with_devices();
        assert_eq!(state.settings_row_count(), SETTINGS_FIXED_ROWS + 2);
        assert_eq!(state.settings_kind(0), Some(SettingsRow::LocalEcho));
        assert_eq!(state.settings_kind(2), Some(SettingsRow::ShareAddress));
        assert_eq!(state.settings_kind(3), Some(SettingsRow::SharePort));
        assert_eq!(state.settings_kind(4), Some(SettingsRow::Device(0)));
        assert_eq!(state.settings_kind(5), Some(SettingsRow::Device(1)));
        assert_eq!(state.settings_kind(6), None);
    }

    #[test]
    fn the_connect_form_masks_secrets_and_pastes_a_whole_key_in_one_action() {
        let mut form = ConnectForm::new();
        assert_eq!(form.fields(), &ConnectField::NORMAL);
        form.address = "192.0.2.10".into();
        form.port = "7331".into();
        form.field = ConnectField::Key;
        let key = format!("snx1.human.{}", "A".repeat(120));
        form.push_str(&key);
        assert_eq!(form.key, key);
        assert_eq!(form.value(ConnectField::Key), "••••••••");
        assert!(!form.value(ConnectField::Key).contains("snx1"));

        form.toggle_mode();
        assert_eq!(form.mode, FormMode::Advanced);
        assert_eq!(form.field, ConnectField::Address);
        assert_eq!(form.address, "192.0.2.10", "the address carries over");
        assert_eq!(form.port, "7331", "the port carries over");
        assert_eq!(form.fields(), &ConnectField::ADVANCED);
        form.toggle_mode();
        assert_eq!(form.fields(), &ConnectField::NORMAL);
        assert_eq!(form.key, key, "the key is kept while the mode changes");
    }

    #[test]
    fn form_pastes_are_strict_for_keys_and_bounded() {
        let mut form = ConnectForm::new();
        form.field = ConnectField::Key;
        form.push_str("  snx1.human.abc\r\n");
        assert_eq!(form.key, "snx1.human.abc", "surrounding whitespace is fine");
        assert!(form.error.is_none());

        // A key is never silently repaired: an inner space or a non-ASCII
        // character refuses the whole paste and leaves the field alone.
        let before = form.key.clone();
        form.push_str("snx1.human.a bc");
        assert_eq!(form.key, before);
        assert!(form.error.as_deref().unwrap_or("").contains("paste"));
        form.push_str("snx1.human.abc\u{200b}");
        assert_eq!(form.key, before);
        form.error = None;
        form.push_str("snx1.human.abc");
        assert_eq!(form.key, format!("{before}snx1.human.abc"));

        // Typed characters are strict too: a space is reported, not dropped,
        // and it blocks submission until the field is started again.
        form.error = None;
        form.push(' ');
        assert_eq!(form.key, format!("{before}snx1.human.abc"));
        assert!(form.key_invalid);
        assert!(form.error.as_deref().unwrap_or("").contains("printable"));
        for _ in 0..form.key.len() {
            form.backspace();
        }
        assert!(!form.key_invalid, "an emptied field starts again");

        // An oversized paste is refused, never truncated into another value.
        let before = form.key.clone();
        form.push_str(&"A".repeat(MAX_FORM_FIELD_BYTES + 100));
        assert_eq!(form.key, before);
        assert!(form.error.as_deref().unwrap_or("").contains("too long"));

        let mut mcp = McpPairForm::new();
        mcp.field = McpField::Key;
        mcp.push_str("snx1.ai.abc\r\n");
        assert_eq!(mcp.key, "snx1.ai.abc");
        assert_eq!(mcp.value(McpField::Key), "••••••••");
        assert!(!mcp.confirm_replace);
        mcp.push_str("snx1.ai.a b");
        assert_eq!(mcp.key, "snx1.ai.abc");
    }

    #[test]
    fn editing_another_field_does_not_clear_a_rejected_key() {
        let mut connect = ConnectForm::new();
        connect.field = ConnectField::Key;
        connect.push(' ');
        assert!(connect.key_invalid);
        connect.field = ConnectField::Port;
        connect.backspace();
        assert!(
            connect.key_invalid,
            "editing the port must not hide a rejected key"
        );
        connect.field = ConnectField::Key;
        connect.backspace();
        assert!(!connect.key_invalid, "clearing the key starts it over");

        let mut mcp = McpPairForm::new();
        mcp.field = McpField::Key;
        mcp.push('\u{00a0}');
        assert!(mcp.key_invalid);
        mcp.field = McpField::Address;
        mcp.backspace();
        assert!(mcp.key_invalid);
        mcp.field = McpField::Key;
        mcp.backspace();
        assert!(!mcp.key_invalid);
    }

    #[test]
    fn a_clipboard_copy_never_prints_the_key() {
        let copy = ClipboardCopy {
            label: "human terminal key",
            key: "snx1.human.super-secret-key".to_string(),
        };
        let debug = format!("{copy:?}");
        assert!(!debug.contains("super-secret-key"), "{debug}");
        assert!(debug.contains("redacted"), "{debug}");
    }

    #[test]
    fn editing_an_mcp_form_cancels_a_pending_replace_confirmation() {
        let mut form = McpPairForm::new();
        form.confirm_replace = true;
        form.push('1');
        assert!(
            !form.confirm_replace,
            "a change asks for confirmation again"
        );
        form.confirm_replace = true;
        form.next_field(1);
        assert!(form.confirm_replace, "moving fields is not an edit");
        form.backspace();
        assert!(!form.confirm_replace);
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(24, 80)
    }

    #[test]
    fn an_ordinary_notice_clears_after_six_seconds_and_not_before() {
        let mut state = state();
        let start = Instant::now();
        state.set_notice_at(Notice::error("old failure"), start);
        state.expire_notices(start + Duration::from_secs(5));
        assert!(state.notice.is_some(), "still readable before its time");
        state.expire_notices(start + NOTICE_TTL);
        assert!(
            state.notice.is_none(),
            "the stale failure no longer covers a new action"
        );
    }

    #[test]
    fn a_serious_warning_survives_ordinary_notices_until_dismissed() {
        let mut state = state();
        let start = Instant::now();
        state.set_notice_at(Notice::warning("write may have reached the device"), start);
        state.set_notice_at(Notice::info("opened /dev/a"), start);
        state.expire_notices(start + Duration::from_secs(5));
        let visible = state.notice.as_ref().expect("the warning is still visible");
        assert!(visible.sticky);
        assert!(visible.text.contains("write may have reached"));

        assert!(state.dismiss_warning());
        let deferred = state.notice.as_ref().expect("the ordinary notice follows");
        assert!(!deferred.sticky);
        assert!(deferred.text.contains("opened /dev/a"));
    }

    #[test]
    fn multiple_warnings_are_shown_one_at_a_time_and_counted() {
        let mut state = state();
        let now = Instant::now();
        state.set_notice_at(Notice::warning("first warning"), now);
        state.set_notice_at(Notice::warning("second warning"), now);
        assert_eq!(state.warning_position(), Some((1, 2)));
        assert_eq!(state.notice.as_ref().unwrap().text, "first warning");

        assert!(state.dismiss_warning());
        assert_eq!(state.warning_position(), Some((1, 1)));
        assert_eq!(state.notice.as_ref().unwrap().text, "second warning");
        assert!(state.dismiss_warning());
        assert!(!state.has_warning());
        assert!(state.notice.is_none());
    }

    #[test]
    fn opening_settings_or_the_share_page_keeps_a_serious_warning() {
        let mut state = AppState::new(30, 100);
        state.warning("write uncertain");
        state.begin_settings();
        assert!(state.has_warning(), "Settings must not erase a warning");
        assert_eq!(state.notice.as_ref().unwrap().text, "write uncertain");
        state.cancel_settings();
        state.begin_share();
        assert!(
            state.has_warning(),
            "the Share page must not erase it either"
        );
    }

    #[test]
    fn a_warning_row_does_not_move_the_share_or_connect_button() {
        let mut state = AppState::new(30, 100);
        state.warning("write uncertain");
        state.select_sidebar_button(SidebarButton::Share);
        assert_eq!(state.selected_button(), Some(SidebarButton::Share));
        state.select_sidebar_button(SidebarButton::Connect);
        assert_eq!(state.selected_button(), Some(SidebarButton::Connect));
    }

    #[test]
    fn a_successful_corrective_action_clears_an_ordinary_notice_but_not_a_warning() {
        let mut state = state();
        state.info("settings not saved");
        state.clear_ordinary_notice();
        assert!(state.notice.is_none());

        state.warning("write uncertain");
        state.clear_ordinary_notice();
        assert!(state.has_warning(), "a warning needs explicit dismissal");
    }
}

#[cfg(test)]
mod share_page_tests {
    use super::*;

    fn state() -> AppState {
        let mut state = AppState::new(30, 100);
        state.set_devices(vec![
            DeviceEntry::new("/dev/ttyUSB0"),
            DeviceEntry::new("/dev/ttyUSB1"),
        ]);
        state
    }

    #[test]
    fn opening_the_share_page_never_starts_sharing_and_starts_from_the_effective_values() {
        let mut state = state();
        state.settings.share_address = "127.0.0.1".into();
        state.settings.share_port = 7331;
        state.begin_share();
        assert_eq!(state.view, View::Share);
        assert!(!state.sharing.active);
        assert_eq!(state.draft.share_address, "127.0.0.1");
        assert_eq!(state.port_text, "7331");
        assert!(!state.has_unsaved_share());
    }

    #[test]
    fn share_edits_are_dirty_until_cancelled_or_saved_and_buttons_appear() {
        let mut state = state();
        state.begin_share();
        state.share_row = 1; // Port
        assert!(state.begin_share_edit());
        state.share_edit_push('7');
        state.end_share_edit();
        assert!(state.has_unsaved_share());
        assert!(state.share_rows().contains(&ShareRow::Save));
        assert!(state.share_rows().contains(&ShareRow::Cancel));

        state.cancel_share_edits();
        assert!(!state.has_unsaved_share());
        assert_eq!(state.port_text, state.settings.share_port.to_string());
    }

    #[test]
    fn checking_a_device_on_the_share_page_keeps_its_identity() {
        let mut state = state();
        state.begin_share();
        state.share_row = 2; // first device
        assert_eq!(state.toggle_share_draft(), Some(true));
        assert!(state.draft_checked.contains("/dev/ttyUSB0"));
        assert!(state
            .draft_checked
            .identities()
            .contains_key("/dev/ttyUSB0"));
        assert_eq!(state.toggle_share_draft(), Some(false));
        assert!(!state.draft_checked.contains("/dev/ttyUSB0"));
    }

    #[test]
    fn a_compact_warning_moves_the_highlight_to_its_visible_dismiss_row() {
        // These are the body dimensions the event loop stores, not the whole
        // terminal with its header and footer.
        let mut state = AppState::new(3, 40);
        state.set_devices(vec![DeviceEntry::new("/dev/ttyUSB0")]);
        state.select_index(0);
        assert_eq!(state.selected_button(), None);
        assert!(
            crate::render::short_warning_shares_list(3, 40),
            "compact layout"
        );
        state.warning("write uncertain");
        assert_eq!(
            state.selected_button(),
            Some(SidebarButton::Dismiss),
            "Enter must act on the label that covers the device row"
        );
    }

    #[test]
    fn moving_the_share_selection_keeps_an_action_failure_visible() {
        let mut state = state();
        state.begin_share();
        state.share_error = Some("no settings file".into());
        state.move_share_row(1);
        assert_eq!(
            state.share_error.as_deref(),
            Some("no settings file"),
            "navigating is not a correction or a dismissal"
        );
    }

    #[test]
    fn discovery_keeps_the_selected_share_device_by_path() {
        let mut state = state();
        state.begin_share();
        state.share_row = 3; // the second device row
        assert_eq!(state.selected_share_row(), ShareRow::Device(1));

        // Removing the earlier device renumbers the survivor; the selection
        // follows the same device instead of landing on Start/Stop.
        state.set_devices(vec![DeviceEntry::new("/dev/ttyUSB1")]);
        assert_eq!(state.selected_share_row(), ShareRow::Device(0));

        // A removed selected device falls back to the safe Address row.
        state.set_devices(Vec::new());
        assert_eq!(state.selected_share_row(), ShareRow::Address);
    }

    #[test]
    fn a_bracketed_paste_fills_a_share_endpoint_field() {
        let mut state = state();
        state.begin_share();
        state.share_row = 0; // Address
        assert!(state.begin_share_edit());
        state.share_edit_push_str("127.0.0.2\n");
        assert_eq!(state.draft.share_address, "127.0.0.2");
        state.end_share_edit();

        state.share_row = 1; // Port
        assert!(state.begin_share_edit());
        state.share_edit_push_str("7444");
        assert_eq!(state.port_text, "7444");
    }

    #[test]
    fn clearing_a_connect_key_keeps_the_endpoint_and_forgets_the_invalid_paste() {
        let mut form = ConnectForm::new();
        form.address = "192.0.2.1".into();
        form.port = "7331".into();
        form.field = ConnectField::Key;
        form.push_str("snx1.human.not a key");
        assert!(form.key_invalid);
        form.clear_key();
        assert!(form.key.is_empty());
        assert!(!form.key_invalid);
        assert_eq!(form.address, "192.0.2.1");
        assert_eq!(form.port, "7331");
    }
}
