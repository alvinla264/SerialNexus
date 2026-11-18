//! Drawing the TUI into a ratatui buffer. No terminal is involved, so the
//! render seam is tested by inspecting the buffer.
//!
//! One layout function decides where everything sits, including the cells the
//! mouse can click, so drawing, resizing the stream screen and hit testing
//! always agree.
//!
//! At a usable width the sidebar and the right pane are drawn side by side:
//! the sidebar on the left, the shown device's stream (or its scrollback, or
//! the settings) on the right. A narrow terminal cannot show both, so it shows
//! the focused pane and `Ctrl+]` switches which one that is.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use serial_nexus_protocol::SharingKey;

use crate::screen::{Color, Style};
use crate::state::{
    AppState, ConnectField, EndpointId, Focus, FormMode, McpField, SettingsRow, Severity,
    SidebarButton, SidebarRow, View, LOCAL_ENDPOINT,
};

/// The sidebar's preferred narrowest width when both panes are shown.
pub const MIN_SIDEBAR_WIDTH: u16 = 20;
/// The right pane needs this much room to be usable next to the sidebar.
pub const MIN_RIGHT_WIDTH: u16 = 40;
/// The sidebar never grows past this, however wide the terminal is.
pub const MAX_SIDEBAR_WIDTH: u16 = 36;
/// The column between the two panes.
const SEPARATOR_WIDTH: u16 = 1;

/// Where each part of the interface sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chrome {
    pub header: Rect,
    /// The area the panes share: the sidebar and the right pane.
    pub main: Rect,
    /// One or two rows at the bottom: the status line and the key hints.
    pub footer: Rect,
}

/// Compute the layout for `area`. Zero-height rects mean "not drawn".
pub fn chrome(area: Rect) -> Chrome {
    if area.width == 0 || area.height == 0 {
        let empty = Rect::new(area.x, area.y, 0, 0);
        return Chrome {
            header: empty,
            main: empty,
            footer: empty,
        };
    }
    let header = Rect::new(area.x, area.y, area.width, 1);
    let footer_height = match area.height {
        1 => 0,
        2..=5 => 1,
        _ => 2,
    };
    let main_height = area.height.saturating_sub(1 + footer_height);
    let main = Rect::new(area.x, area.y + 1, area.width, main_height);
    let footer = if footer_height == 0 {
        Rect::new(area.x, area.y + 1, area.width, 0)
    } else {
        Rect::new(
            area.x,
            area.y + area.height - footer_height,
            area.width,
            footer_height,
        )
    };
    Chrome {
        header,
        main,
        footer,
    }
}

/// Where the sidebar and the right pane sit in this terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Panes {
    /// Whether both panes are shown. When false, the sidebar and right rects
    /// are the same full area and only the focused one is drawn.
    pub split: bool,
    pub sidebar: Rect,
    /// The right pane: the stream's drawing area, or the settings/scrollback.
    /// It keeps this size even when a narrow terminal hides it, so moving the
    /// focus or resizing never reshapes a session's screen.
    pub right: Rect,
    pub separator: Option<Rect>,
}

/// Compute the panes for `area`, independently of which pane has the focus.
pub fn panes(area: Rect) -> Panes {
    let main = chrome(area).main;
    if main.width == 0 || main.height == 0 {
        let empty = Rect::new(main.x, main.y, 0, 0);
        return Panes {
            split: false,
            sidebar: empty,
            right: empty,
            separator: None,
        };
    }
    if main.width < MIN_SIDEBAR_WIDTH + SEPARATOR_WIDTH + MIN_RIGHT_WIDTH {
        return Panes {
            split: false,
            sidebar: main,
            right: main,
            separator: None,
        };
    }
    let room = main.width.saturating_sub(MIN_RIGHT_WIDTH + SEPARATOR_WIDTH);
    let width = (main.width * 2 / 5)
        .clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH)
        .min(room);
    let sidebar = Rect::new(main.x, main.y, width, main.height);
    let separator = Rect::new(main.x + width, main.y, SEPARATOR_WIDTH, main.height);
    let right = Rect::new(
        separator.x + SEPARATOR_WIDTH,
        main.y,
        main.width - width - SEPARATOR_WIDTH,
        main.height,
    );
    Panes {
        split: true,
        sidebar,
        right,
        separator: Some(separator),
    }
}

/// The stream's drawing area: the right pane, whatever has the focus. Sessions
/// are sized to it even when a narrow terminal is showing the sidebar.
pub fn stream_body_area(area: Rect) -> Option<Rect> {
    let right = panes(area).right;
    (right.width > 0 && right.height > 0).then_some(right)
}

/// The sidebar's rows: a title, the device list, and the Share, Connect and
/// Settings buttons, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sidebar {
    pub title: Rect,
    pub list: Rect,
    pub dismiss: Rect,
    pub share: Rect,
    pub connect: Rect,
    pub button: Rect,
}

/// Where the sidebar puts its parts inside `rect`, or `None` when there is no
/// room.
pub(crate) fn available_sidebar_buttons(
    rows: usize,
    cols: usize,
    has_warning: bool,
) -> Vec<SidebarButton> {
    let rect = Rect::new(
        0,
        0,
        cols.min(u16::MAX as usize) as u16,
        rows.min(u16::MAX as usize) as u16,
    );
    let Some(layout) = sidebar_with_warning(rect, has_warning) else {
        return Vec::new();
    };
    let mut buttons = Vec::new();
    if layout.dismiss.height > 0 {
        buttons.push(SidebarButton::Dismiss);
    }
    if layout.share.height > 0 {
        buttons.push(SidebarButton::Share);
    }
    if layout.connect.height > 0 {
        buttons.push(SidebarButton::Connect);
    }
    if layout.button.height > 0 {
        buttons.push(SidebarButton::Settings);
    }
    buttons
}

pub fn sidebar(rect: Rect) -> Option<Sidebar> {
    sidebar_with_warning(rect, false)
}

pub fn sidebar_with_warning(rect: Rect, has_warning: bool) -> Option<Sidebar> {
    if rect.height < 2 || rect.width < 8 {
        return None;
    }
    let empty = Rect::new(rect.x, rect.y, 0, 0);
    let title = Rect::new(rect.x, rect.y, rect.width, 1);
    let bottom = rect.y + rect.height - 1;
    let button = Rect::new(rect.x, bottom, rect.width, 1);
    // Short sidebars give rows to the controls first. Settings survives at the
    // old two-row minimum; Share, Connect and the warning's Dismiss entry
    // appear only when there is room. Which controls exist is decided from the
    // same layout by `available_sidebar_buttons`, so drawing, clicking and the
    // arrow keys never disagree.
    let (mut dismiss, share, connect, list_height) = if has_warning && rect.height >= 5 {
        (
            Rect::new(rect.x, bottom - 3, rect.width, 1),
            Rect::new(rect.x, bottom - 2, rect.width, 1),
            Rect::new(rect.x, bottom - 1, rect.width, 1),
            rect.height - 5,
        )
    } else if rect.height >= 4 {
        (
            empty,
            Rect::new(rect.x, bottom - 2, rect.width, 1),
            Rect::new(rect.x, bottom - 1, rect.width, 1),
            rect.height - 4,
        )
    } else {
        (empty, empty, empty, rect.height - 2)
    };
    let list = Rect::new(rect.x, rect.y + 1, rect.width, list_height);
    // A genuinely short sidebar still offers a mouse target for a serious
    // warning. At three rows the spare list row is it; with only a title and
    // Settings, the title row becomes the Dismiss button.
    if has_warning && dismiss.height == 0 {
        dismiss = if rect.height == 3 { list } else { title };
    }
    Some(Sidebar {
        title,
        list,
        dismiss,
        share,
        connect,
        button,
    })
}

/// The Settings button's row, for the mouse and for tests.
pub fn settings_button(area: Rect) -> Option<Rect> {
    sidebar(panes(area).sidebar).map(|sidebar| sidebar.button)
}

/// The settings view's parts: a title, the flag rows, a bar with the Save and
/// Cancel buttons, and (when there is room) the key hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingsLayout {
    pub title: Rect,
    pub rows: Rect,
    pub bar: Rect,
    pub save: Rect,
    pub cancel: Rect,
    pub hints: Rect,
}

/// What a click in the settings view landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsHit {
    Flag(usize),
    Save,
    Cancel,
}

/// Where the settings view puts its parts inside the right pane, or `None`
/// when there is no room. The Save and Cancel buttons get their own row: the
/// settings are only kept by an explicit action, so it is always visible when
/// the view is drawn.
pub fn settings_layout(right: Rect) -> Option<SettingsLayout> {
    if right.height < 2 || right.width < 12 {
        return None;
    }
    let hint_height = if right.height >= 5 { 1 } else { 0 };
    // A title and a bar of buttons do not both fit in a very short area: the
    // flags and the Save button win, the title gives way first.
    let title_height = if right.height >= 4 { 1 } else { 0 };
    let title = Rect::new(right.x, right.y, right.width, title_height);
    let bar = Rect::new(
        right.x,
        right.y + right.height - 1 - hint_height,
        right.width,
        1,
    );
    let hints = Rect::new(
        right.x,
        right.y + right.height - hint_height,
        right.width,
        hint_height,
    );
    let rows = Rect::new(
        right.x,
        right.y + title_height,
        right.width,
        right.height - title_height - 1 - hint_height,
    );
    // Reserve enough room for `[ Confirm ]` as well as `[ Save ]`.
    let save_width = 11.min(bar.width.saturating_sub(3));
    let cancel_width = 10;
    let save = Rect::new(bar.x + 1, bar.y, save_width, 1);
    let cancel_x = save.x + save.width + 2;
    let cancel = Rect::new(
        cancel_x,
        bar.y,
        cancel_width.min(bar.x.saturating_add(bar.width).saturating_sub(cancel_x)),
        1,
    );
    Some(SettingsLayout {
        title,
        rows,
        bar,
        save,
        cancel,
        hints,
    })
}

/// Hit test one click in the settings view, which the right pane holds.
pub fn settings_hit_test(
    state: &AppState,
    right: Rect,
    column: u16,
    row: u16,
) -> Option<SettingsHit> {
    let layout = settings_layout(right)?;
    if in_rect(layout.save, column, row) {
        return Some(SettingsHit::Save);
    }
    if in_rect(layout.cancel, column, row) {
        return Some(SettingsHit::Cancel);
    }
    if in_rect(layout.rows, column, row) {
        let index = (row - layout.rows.y) as usize + settings_first_row(state, layout);
        return (index < state.settings_row_count()).then_some(SettingsHit::Flag(index));
    }
    None
}

/// The first flag the rows show, so the selected one stays in view.
fn settings_first_row(state: &AppState, layout: SettingsLayout) -> usize {
    let rows = state.settings_row_count().max(1);
    let visible = layout.rows.height.max(1) as usize;
    let last_first = rows.saturating_sub(visible);
    state
        .settings_row
        .min(rows - 1)
        .saturating_sub(visible.saturating_sub(1))
        .min(last_first)
}

/// What a click in the sidebar landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Device(usize),
    Dismiss,
    Share,
    Connect,
    Settings,
}

/// Where the Share page puts its parts inside the right pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareLayout {
    pub title: Rect,
    /// The editable rows and explicit buttons.
    pub form: Rect,
    /// The running session's keys and advanced host fields.
    pub details: Rect,
    /// The in-form reason a save or start was refused.
    pub error: Option<Rect>,
    pub hints: Rect,
}

fn share_layout(state: &AppState, right: Rect) -> Option<ShareLayout> {
    if right.height < 2 || right.width < 12 {
        return None;
    }
    let title_height = if right.height >= 3 { 1 } else { 0 };
    let hints_height = if right.height >= 5 { 1 } else { 0 };
    let error_height = u16::from(
        (state.share_error.is_some()
            || state.share_confirm == Some(crate::state::ShareConfirm::Rebind))
            && right.height >= 2,
    );
    let content_height = right
        .height
        .saturating_sub(title_height + hints_height + error_height);
    let rows = state.share_rows().len() as u16;
    // Saving all the controls must never make the keys unreachable. While a
    // session is running, reserve at least three rows for its keys/details;
    // the form scrolls to keep the selected row visible.
    let detail_reserve = if state.sharing.active && state.sharing.pairing.is_some() {
        content_height.saturating_sub(1).min(3)
    } else {
        0
    };
    let form_height = rows.min(content_height.saturating_sub(detail_reserve).max(1));
    let title = Rect::new(right.x, right.y, right.width, title_height);
    let form = Rect::new(right.x, right.y + title_height, right.width, form_height);
    let details_y = form.y + form.height;
    let error = (error_height > 0).then(|| {
        Rect::new(
            right.x,
            right.y + title_height + content_height,
            right.width,
            1,
        )
    });
    let details_height = content_height.saturating_sub(form_height);
    let details = Rect::new(right.x, details_y, right.width, details_height);
    let hints = Rect::new(
        right.x,
        right.y + right.height - hints_height,
        right.width,
        hints_height,
    );
    Some(ShareLayout {
        title,
        form,
        details,
        error,
        hints,
    })
}

/// Whether a serious warning's compact Dismiss target shares the selected
/// device's list row, so keyboard activation must dismiss instead of opening
/// the device hidden under the label.
pub fn short_warning_shares_list(rows: usize, cols: usize) -> bool {
    // Callers pass the body/sidebar dimensions, not the whole terminal, so
    // this must not subtract the chrome a second time.
    let sidebar = Rect::new(
        0,
        0,
        cols.min(u16::MAX as usize) as u16,
        rows.min(u16::MAX as usize) as u16,
    );
    let Some(layout) = sidebar_with_warning(sidebar, true) else {
        return false;
    };
    layout.dismiss.height > 0
        && layout.list.height > 0
        && layout.dismiss.y >= layout.list.y
        && layout.dismiss.y < layout.list.y + layout.list.height
}

/// Which Share-page area a mouse wheel notch is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareWheel {
    Form,
    Details,
}

/// Resolve a wheel notch's pointer to the Share form or its key/details area.
pub fn share_wheel_target(
    state: &AppState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<ShareWheel> {
    let layout = share_layout(state, panes(area).right)?;
    if in_rect(layout.form, column, row) {
        Some(ShareWheel::Form)
    } else if in_rect(layout.details, column, row) {
        Some(ShareWheel::Details)
    } else {
        None
    }
}

/// Hit test one click in the Share page's form. Returns the row index in
/// `AppState::share_rows`.
pub fn share_hit_test(state: &AppState, right: Rect, column: u16, row: u16) -> Option<usize> {
    let layout = share_layout(state, right)?;
    // An error drawn over the last content row is not a hidden button.
    if layout.error.is_none()
        && state.share_error.is_some()
        && right.height > 0
        && row == right.y + right.height - 1
    {
        return None;
    }
    if !in_rect(layout.form, column, row) {
        return None;
    }
    let first = share_first_row(state, layout.form.height as usize);
    let index = first + (row - layout.form.y) as usize;
    (index < state.share_rows().len()).then_some(index)
}

fn share_first_row(state: &AppState, visible: usize) -> usize {
    let rows = state.share_rows().len().max(1);
    let visible = visible.max(1);
    state
        .share_row
        .min(rows - 1)
        .saturating_sub(visible.saturating_sub(1))
        .min(rows.saturating_sub(visible))
}

/// Hit test one click against the sidebar as it is drawn. The sidebar takes
/// clicks when it is split beside the right pane, or when it is the focused
/// pane of a narrow terminal.
pub fn hit_test(state: &AppState, area: Rect, column: u16, row: u16) -> Option<Hit> {
    let panes = panes(area);
    if !panes.split && state.focus != Focus::Sidebar {
        return None;
    }
    let sidebar = sidebar_with_warning(panes.sidebar, state.has_warning())?;
    if in_rect(sidebar.button, column, row) {
        return Some(Hit::Settings);
    }
    if in_rect(sidebar.dismiss, column, row) {
        return Some(Hit::Dismiss);
    }
    if in_rect(sidebar.share, column, row) {
        return Some(Hit::Share);
    }
    if in_rect(sidebar.connect, column, row) {
        return Some(Hit::Connect);
    }
    let empty_endpoints = state.empty_remote_endpoints();
    let sticky_rows = sticky_empty_rows(
        empty_endpoints.len(),
        sidebar.list.height as usize,
        !state.devices.is_empty(),
    );
    let list = sidebar_device_list(sidebar, sticky_rows);
    if list.height == 0 || !in_rect(list, column, row) {
        return None;
    }
    let rows: Vec<SidebarRow> = state
        .sidebar_rows()
        .into_iter()
        .filter(|row| !is_empty_remote_group(&empty_endpoints, *row))
        .collect();
    let list_sidebar = Sidebar { list, ..sidebar };
    let first = first_sidebar_row(state, &rows, list_sidebar);
    let offset = (row - list.y) as usize + first;
    match rows.get(offset) {
        Some(SidebarRow::Device(index)) => Some(Hit::Device(*index)),
        _ => None,
    }
}

/// The first sidebar row the list shows, so the highlighted device stays in
/// view together with its group heading where possible.
fn first_sidebar_row(state: &AppState, rows: &[SidebarRow], sidebar: Sidebar) -> usize {
    let capacity = sidebar.list.height.max(1) as usize;
    let selected = rows
        .iter()
        .position(|row| matches!(row, SidebarRow::Device(index) if *index == state.selected))
        .unwrap_or(0);
    // Clamp to the list's own last page so trailing rows (for example a
    // connected host with no approved devices) can be seen from the last
    // selectable device.
    let first = selected.saturating_add(1).saturating_sub(capacity);
    let last_device = rows
        .iter()
        .rposition(|row| matches!(row, SidebarRow::Device(_)));
    let last_page = rows.len().saturating_sub(capacity);
    // Advance for a trailing empty-host heading only when that page still
    // contains the selected device. On a very short list, keep the selectable
    // device visible; headers must never steal the cursor from its selection.
    if Some(selected) == last_device && last_page <= selected {
        first.max(last_page)
    } else {
        first
    }
}

fn is_empty_remote_group(empty_endpoints: &[EndpointId], row: SidebarRow) -> bool {
    matches!(row, SidebarRow::Group(endpoint) if empty_endpoints.contains(&endpoint))
}

fn sticky_empty_rows(empty_count: usize, list_height: usize, has_devices: bool) -> usize {
    let available = if has_devices {
        list_height.saturating_sub(1)
    } else {
        list_height
    };
    empty_count.min(available)
}

fn sidebar_device_list(sidebar: Sidebar, sticky_rows: usize) -> Rect {
    let sticky_rows = sticky_rows.min(sidebar.list.height as usize) as u16;
    Rect::new(
        sidebar.list.x,
        sidebar.list.y.saturating_add(sticky_rows),
        sidebar.list.width,
        sidebar.list.height.saturating_sub(sticky_rows),
    )
}

fn in_rect(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Draw the whole UI into `buffer`, using `area` as the screen, and return
/// where the terminal cursor goes.
pub fn render(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    buffer.reset();
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let chrome = chrome(area);
    draw_header(state, buffer, chrome.header);
    let panes = panes(area);
    let cursor = if panes.split {
        let sidebar_cursor = draw_sidebar(state, buffer, panes.sidebar);
        draw_separator(state, buffer, panes.separator);
        let right_cursor = draw_right(state, buffer, panes.right);
        if state.focus == Focus::Sidebar {
            sidebar_cursor
        } else {
            right_cursor
        }
    } else {
        match state.focus {
            Focus::Sidebar => draw_sidebar(state, buffer, panes.sidebar),
            Focus::Right => draw_right(state, buffer, panes.right),
        }
    };
    draw_footer(state, buffer, chrome.footer);
    cursor
}

/// The right pane holds the stream, its scrollback, or the settings.
fn draw_right(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    match state.view {
        View::Stream => draw_stream(state, buffer, area),
        View::Scrollback => draw_stream(state, buffer, area),
        View::Settings => draw_settings(state, buffer, area),
        View::Share => draw_share(state, buffer, area),
        View::Connect => draw_connect(state, buffer, area),
        View::McpPair => draw_mcp_pair(state, buffer, area),
    }
}

fn draw_separator(state: &AppState, buffer: &mut Buffer, separator: Option<Rect>) {
    let Some(separator) = separator else {
        return;
    };
    let style = if state.focus == Focus::Right {
        RStyle::default()
            .fg(RColor::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        dim()
    };
    for row in 0..separator.height {
        put_cell(buffer, separator.x, separator.y + row, "│", style);
    }
}

fn draw_header(state: &AppState, buffer: &mut Buffer, area: Rect) {
    if area.height == 0 {
        return;
    }
    let device = state.shown_device();
    let path = device
        .map(|device| device.path.as_str())
        .unwrap_or("no device shown");
    let baud = device
        .and_then(|device| device.baud)
        .map(|baud| baud.to_string())
        .unwrap_or_else(|| "-".to_string());
    let session = state.shown_session();
    let control = session
        .and_then(|session| session.control.as_deref())
        .or_else(|| device.and_then(|device| device.control.as_deref()))
        .unwrap_or("none");
    let attached = if session.is_some() {
        "attached"
    } else {
        "detached"
    };
    let view = match (state.focus, state.view) {
        (Focus::Sidebar, _) => "focus: sidebar",
        (Focus::Right, View::Stream) => "focus: stream",
        (Focus::Right, View::Settings) => "focus: settings",
        (Focus::Right, View::Scrollback) => "focus: scrollback",
        (Focus::Right, View::Share) => "focus: share",
        (Focus::Right, View::Connect) => "focus: connect",
        (Focus::Right, View::McpPair) => "focus: MCP pairing",
    };

    let brand = " SerialNexus ";
    let brand_width = display_width(brand);
    let endpoint = device
        .map(|device| state.endpoint_tag(device.endpoint))
        .unwrap_or_else(|| "local".to_string());
    let details =
        format!("  [{endpoint}] {path}  {baud} baud  AI control: {control}  [{attached}]");
    let right = format!("{view} ");
    let right_width = display_width(&right);
    let hint_fits = area.width.saturating_sub(right_width) >= brand_width + display_width(&details);
    let left_width = if hint_fits {
        area.width - right_width
    } else {
        area.width
    };
    let brand_style = RStyle::default()
        .fg(RColor::Black)
        .bg(RColor::Gray)
        .add_modifier(Modifier::BOLD);
    put(
        buffer,
        Rect::new(area.x, area.y, left_width, 1),
        0,
        brand,
        brand_style,
    );
    put(
        buffer,
        Rect::new(area.x, area.y, left_width, 1),
        brand_width,
        &details,
        RStyle::default().add_modifier(Modifier::BOLD),
    );
    if hint_fits {
        let style = if state.focus == Focus::Right {
            RStyle::default()
                .fg(RColor::Black)
                .bg(RColor::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        put(
            buffer,
            Rect::new(area.x + area.width - right_width, area.y, right_width, 1),
            0,
            &right,
            style,
        );
    }
}

/// The sidebar: every device, then the Settings button, then room for more
/// devices than fit. The highlighted row and the row shown in the right pane
/// are marked differently, so choosing a row never looks like switching.
fn draw_sidebar(state: &AppState, buffer: &mut Buffer, rect: Rect) -> Option<(u16, u16)> {
    let sidebar = sidebar_with_warning(rect, state.has_warning())?;
    let list = sidebar.list;
    let empty_endpoints = state.empty_remote_endpoints();
    let empty_remote_tags: Vec<String> = empty_endpoints
        .iter()
        .map(|endpoint| state.endpoint_tag(*endpoint))
        .collect();
    let sticky_rows = sticky_empty_rows(
        empty_endpoints.len(),
        list.height as usize,
        !state.devices.is_empty(),
    );
    let device_list = sidebar_device_list(sidebar, sticky_rows);
    // With no room for a device row, the title row carries the highlighted
    // device instead, while still showing connected empty hosts.
    let title = if device_list.height == 0 && state.selected_device().is_some() {
        let selected = match state.selected_device() {
            Some(device) if device.endpoint != LOCAL_ENDPOINT => {
                format!("› {} {}", state.endpoint_tag(device.endpoint), device.path)
            }
            Some(device) => format!("› {}", device.path),
            None => " Devices ".to_string(),
        };
        if empty_remote_tags.is_empty() {
            selected
        } else {
            format!("Empty host: {} · {selected}", empty_remote_tags.join(","))
        }
    } else if empty_remote_tags.is_empty() {
        format!(" Devices ({}) ", state.devices.len())
    } else {
        format!(
            " Empty host: {} · Devices ({}) ",
            empty_remote_tags.join(","),
            state.devices.len()
        )
    };
    let title_style = focus_title_style(state.focus == Focus::Sidebar);
    put(buffer, sidebar.title, 0, &title, title_style);
    for (offset, endpoint) in empty_endpoints.iter().take(sticky_rows).enumerate() {
        put(
            buffer,
            Rect::new(list.x, list.y + offset as u16, list.width, 1),
            0,
            &format!(" {}", state.endpoint_label(*endpoint)),
            dim(),
        );
    }
    if state.devices.is_empty() && device_list.height > 0 {
        put(
            buffer,
            device_list,
            1,
            "no serial devices — is the core running?",
            dim(),
        );
    }
    let shown = state
        .shown_device()
        .map(|device| (device.endpoint, device.path.clone()));
    let rows: Vec<SidebarRow> = state
        .sidebar_rows()
        .into_iter()
        .filter(|row| !is_empty_remote_group(&empty_endpoints, *row))
        .collect();
    let list_sidebar = Sidebar {
        list: device_list,
        ..sidebar
    };
    let first = first_sidebar_row(state, &rows, list_sidebar);
    for (offset, row) in rows.iter().skip(first).enumerate() {
        if offset >= device_list.height as usize {
            break;
        }
        let y = device_list.y + offset as u16;
        let line = Rect::new(list.x, y, list.width, 1);
        match row {
            SidebarRow::Group(endpoint) => {
                let label = state.endpoint_label(*endpoint);
                let tag = state.endpoint_tag(*endpoint);
                put(buffer, line, 0, &format!(" {tag} {label}"), dim());
            }
            SidebarRow::Device(index) => {
                let device = &state.devices[*index];
                let selected = *index == state.selected;
                let shown_here = shown.as_ref().is_some_and(|(endpoint, path)| {
                    *endpoint == device.endpoint && path == &device.path
                });
                let marker = match (selected, shown_here) {
                    (true, true) => "▸",
                    (true, false) => "›",
                    (false, true) => "▹",
                    (false, false) => " ",
                };
                let dot = if device.open { "●" } else { "○" };
                let label = if state.remote_labels.is_empty() {
                    format!("{marker} {dot} {}", device.path)
                } else {
                    // The tag comes before the path so clipping keeps the
                    // endpoint visible even for a long device name.
                    let tag = state.endpoint_tag(device.endpoint);
                    format!("{marker} {dot} {tag} {}", device.path)
                };
                let attached = state.sessions.values().any(|session| {
                    session.endpoint == device.endpoint && session.path == device.path
                });
                let mut status = if attached {
                    device.baud.map(|b| b.to_string()).unwrap_or_default()
                } else if device.open {
                    format!(
                        "other {}",
                        device.baud.map(|b| b.to_string()).unwrap_or_default()
                    )
                } else {
                    "closed".to_string()
                };
                if let Some(control) = &device.control {
                    status.push_str(&format!(" · AI: {control}"));
                }
                if let Some(description) = &device.description {
                    status.push_str(&format!(" · {description}"));
                }
                let style = if selected {
                    highlighted(true)
                } else if shown_here {
                    RStyle::default()
                        .fg(RColor::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    RStyle::default()
                };
                draw_row(buffer, line, &label, &status, style);
            }
        }
    }

    // The buttons are real rows: selected with the arrow keys and clickable
    // with the mouse. A serious warning adds an explicit Dismiss row.
    let mut cursor = draw_sidebar_button(
        buffer,
        sidebar.dismiss,
        state.selected_button() == Some(SidebarButton::Dismiss),
        "[ Dismiss warning ]",
        "x dismiss",
    );
    let share_selected = state.selected_button() == Some(SidebarButton::Share);
    let share_label = if state.sharing.active {
        "[ Share: on ]"
    } else {
        "[ Share: off ]"
    };
    cursor = draw_sidebar_button(
        buffer,
        sidebar.share,
        share_selected,
        share_label,
        "Enter opens · s",
    )
    .or(cursor);
    cursor = draw_sidebar_button(
        buffer,
        sidebar.connect,
        state.selected_button() == Some(SidebarButton::Connect),
        "[ Connect ]",
        "Enter · c",
    )
    .or(cursor);
    cursor = draw_sidebar_button(
        buffer,
        sidebar.button,
        state.settings_button_selected(),
        "[ Settings ]",
        "Enter opens",
    )
    .or(cursor);

    if let Some(cursor) = cursor {
        Some(cursor)
    } else if device_list.height == 0 {
        // The device is shown on the title row when no device row fits.
        Some((sidebar.title.x + 1, sidebar.title.y))
    } else {
        // The highlighted device row, which the list has kept in view.
        let selected_row = rows
            .iter()
            .position(|row| matches!(row, SidebarRow::Device(index) if *index == state.selected))
            .unwrap_or(0);
        let offset = selected_row.saturating_sub(first) as u16;
        Some((device_list.x + 1, device_list.y + offset))
    }
}

/// The settings view in the right pane: two flags, the sharing address and
/// port, and one checkbox per detected device, all edited as a draft.
fn draw_settings(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let Some(layout) = settings_layout(area) else {
        // No room for buttons: still say how to save and leave by keyboard.
        put(buffer, area, 0, " Esc closes · Ctrl+S saves ", dim());
        return None;
    };
    if layout.title.height > 0 {
        let title_style = focus_title_style(state.focus == Focus::Right);
        let title = if state.settings_rebind_confirm {
            " Confirm endpoint change "
        } else {
            " Settings "
        };
        put(
            buffer,
            layout.title,
            0,
            &fit(title, layout.title.width as usize),
            title_style,
        );
    }
    let first = settings_first_row(state, layout);
    let visible = layout.rows.height.max(1) as usize;
    let rows = state.settings_row_count();
    for row in first..rows {
        if row - first >= visible {
            break;
        }
        let label = state.settings_row_label(row);
        let value = state.draft_value(row).unwrap_or_default();
        let selected = row == state.settings_row;
        let marker = if selected { "▸" } else { " " };
        let (label, value) = match state.settings_kind(row) {
            Some(SettingsRow::Device(_)) => {
                let checkbox = if state.draft_device_checked(row) == Some(true) {
                    "[x]"
                } else {
                    "[ ]"
                };
                (format!("{marker} {checkbox} {label}"), String::new())
            }
            Some(SettingsRow::ShareAddress | SettingsRow::SharePort) => {
                let value = if state.settings_editing && selected {
                    format!("{value}│")
                } else {
                    value
                };
                (format!("{marker} {label}"), value)
            }
            _ => (format!("{marker} {label}"), value),
        };
        let line = Rect::new(
            layout.rows.x,
            layout.rows.y + (row - first) as u16,
            layout.rows.width,
            1,
        );
        draw_value_row(buffer, line, &label, &value, highlighted(selected));
    }

    // The obvious way to keep a change: a real button row, clickable and
    // reachable from the keyboard with Ctrl+S.
    let button = RStyle::default()
        .fg(RColor::Black)
        .bg(RColor::Cyan)
        .add_modifier(Modifier::BOLD);
    put(
        buffer,
        layout.save,
        0,
        &fit(
            if state.settings_rebind_confirm {
                "[ Confirm ]"
            } else {
                "[ Save ]"
            },
            layout.save.width as usize,
        ),
        button,
    );
    let quiet = RStyle::default().fg(RColor::Cyan);
    put(
        buffer,
        layout.cancel,
        0,
        &fit("[ Cancel ]", layout.cancel.width as usize),
        quiet,
    );
    if layout.cancel.right() + 2 < layout.bar.right() {
        put(
            buffer,
            Rect::new(
                layout.cancel.right() + 2,
                layout.bar.y,
                layout.bar.right() - (layout.cancel.right() + 2),
                1,
            ),
            0,
            if state.settings_rebind_confirm {
                "Remotes disconnect; fresh long keys"
            } else {
                "Space/Enter toggles · Enter edits address/port · Ctrl+S saves · Esc cancels"
            },
            dim(),
        );
    }
    if layout.hints.height > 0 {
        put(
            buffer,
            layout.hints,
            0,
            " Saved only when the file is written · device checks are session-only ",
            dim(),
        );
    }

    let row = state.settings_row.min(rows.saturating_sub(1));
    let value = state.draft_value(row).unwrap_or_default();
    let value_column = layout.rows.width.saturating_sub(display_width(&value));
    let column = match state.settings_kind(row) {
        Some(SettingsRow::Device(_)) => layout.rows.x + 4,
        _ => layout.rows.x + value_column,
    };
    Some((column, layout.rows.y + (row - first) as u16))
}

/// The Share page's key text, before wrapping. Bounded and secret-bearing:
/// only the owner's Share page draws it.
///
/// The normal view shows two labelled, distinct sharing keys, one per role.
/// The advanced view shows the separate host certificate, fingerprint and
/// credentials an older terminal needs. Neither is ever written anywhere.
fn pairing_lines(state: &AppState) -> Vec<String> {
    let Some(pairing) = &state.sharing.pairing else {
        return Vec::new();
    };
    let snapshot = state.sharing.snapshot.as_ref();
    let checked: Vec<String> = state.sharing.checked().into_iter().collect();
    let remote = snapshot
        .map(|snapshot| snapshot.remote_clients)
        .unwrap_or(0);
    let pending = snapshot
        .map(|snapshot| snapshot.pending_connections)
        .unwrap_or(0);
    let mut lines = vec![
        format!(
            "Sharing on {} · {remote} remote client(s) · {pending} connecting",
            pairing.bind
        ),
        "Firewall and router settings decide who can reach this address; this terminal only listens on it.".to_string(),
        String::new(),
    ];
    if state.pairing_advanced {
        lines.push("Older host fields (for terminals without sharing keys):".to_string());
        lines.push(format!("  host name: {}", pairing.host.host_name));
        lines.push(format!(
            "  certificate SHA-256: {}",
            pairing.host.host_cert_fingerprint_sha256
        ));
        lines.push("  CA certificate (base64 DER):".to_string());
        for chunk in pairing.host.ca_cert_der_b64.as_bytes().chunks(72) {
            lines.push(format!("    {}", String::from_utf8_lossy(chunk)));
        }
        lines.push(String::new());
        lines.push("  human credential:".to_string());
        lines.push(format!("    {}", pairing.human.credential));
        lines.push(String::new());
        lines.push("  MCP (AI) credential:".to_string());
        lines.push(format!("    {}", pairing.ai.credential));
    } else {
        match (
            SharingKey::encode(&pairing.host, &pairing.human),
            SharingKey::encode(&pairing.host, &pairing.ai),
        ) {
            (Ok(human), Ok(ai)) => {
                lines.push(
                    "Human terminal key (with the address and port, into another terminal's Connect form):"
                        .to_string(),
                );
                lines.push(format!("  {human}"));
                lines.push(String::new());
                lines.push(
                    "MCP/AI key (with the address and port, into this terminal's MCP pairing form):"
                        .to_string(),
                );
                lines.push(format!("  {ai}"));
            }
            _ => lines.push(
                "Sharing keys are unavailable for this session; use the Advanced row for older host fields."
                    .to_string(),
            ),
        }
    }
    lines.push(String::new());
    if checked.is_empty() {
        lines.push("Shared devices: none".to_string());
    } else {
        lines.push("Shared devices:".to_string());
        for path in &checked {
            lines.push(format!("  {path}"));
        }
    }
    lines.push(String::new());
    lines.push(
        "Each key is a password: it is shown only here, is never saved or sent to a device, and stops working when sharing stops."
            .to_string(),
    );
    lines.push(
        "A key here is hard-wrapped; selecting those rows adds line breaks and will be refused. Use the visible Copy buttons for the complete keys."
            .to_string(),
    );
    lines.push(
        "If the terminal ignores the clipboard escape (OSC 52 disabled), use a visible Show key button to display one key as a single soft-wrapped line: select that line, then press any key. Enlarge the pane first if it is too short for the whole line."
            .to_string(),
    );
    if state.pairing_advanced {
        lines.push(
            "Esc back · Enter shows/hides older fields · PageUp/PageDown or wheel scrolls the details"
                .to_string(),
        );
    } else {
        lines.push(
            "Esc back · Enter opens older host fields · PageUp/PageDown or wheel scrolls the details"
                .to_string(),
        );
    }

    lines
}

/// The highest useful scroll offset for the Share page's key/details area,
/// sized to the pane it is actually drawn in.
pub fn share_max_scroll(state: &AppState) -> usize {
    // AppState's rows/cols are already the right-pane size.
    let right = Rect::new(
        0,
        0,
        state.cols().min(u16::MAX as usize) as u16,
        state.rows().min(u16::MAX as usize) as u16,
    );
    let Some(layout) = share_layout(state, right) else {
        return 0;
    };
    let width = layout.details.width.max(1) as usize;
    let lines = pairing_lines(state)
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .count();
    lines.saturating_sub(layout.details.height.max(1) as usize)
}

/// The highest useful pairing scroll offset for the current pane size. Keeping
/// navigation clamped stops invisible overscroll from trapping the view.
pub fn pairing_max_scroll(state: &AppState) -> usize {
    let width = state.cols().max(1);
    let lines = pairing_lines(state)
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .count();
    lines.saturating_sub(state.rows().max(1))
}

/// The owner's Share page: the address, port and device checklist with
/// explicit Start/Stop controls, then the running session's keys. Opening the
/// page never acts; every network action is a separate button that may need a
/// second, explicit confirmation.
fn draw_share(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let Some(layout) = share_layout(state, area) else {
        put(
            buffer,
            area,
            0,
            " Esc back · widen the pane to share ",
            dim(),
        );
        return None;
    };
    if layout.title.height > 0 {
        let title = if state.sharing.active {
            format!(
                " Share · on {} ",
                state
                    .sharing
                    .pairing
                    .as_ref()
                    .map(|pairing| pairing.bind.to_string())
                    .or_else(|| state
                        .sharing
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.bind)
                        .map(|bind| bind.to_string()))
                    .unwrap_or_else(|| "running".to_string())
            )
        } else {
            " Share · off ".to_string()
        };
        let title_style = focus_title_style(state.focus == Focus::Right);
        put(buffer, layout.title, 0, &title, title_style);
    }
    let rows = state.share_rows();
    let first = share_first_row(state, layout.form.height as usize);
    let mut cursor = None;
    for (offset, (index, row)) in rows.iter().enumerate().skip(first).enumerate() {
        if offset >= layout.form.height as usize {
            break;
        }
        let selected = index == state.share_row;
        let marker = if selected { "▸" } else { " " };
        let line = Rect::new(
            layout.form.x,
            layout.form.y + offset as u16,
            layout.form.width,
            1,
        );
        let text = match row {
            crate::state::ShareRow::Address => {
                let value = if state.share_editing && selected {
                    format!("{}│", state.draft.share_address)
                } else {
                    state.draft.share_address.clone()
                };
                row_text(
                    &format!("{marker} Host address"),
                    &value,
                    line.width as usize,
                    RowOverflow::Value,
                )
            }
            crate::state::ShareRow::Port => {
                let value = if state.share_editing && selected {
                    format!("{}│", state.port_text)
                } else {
                    state.port_text.clone()
                };
                row_text(
                    &format!("{marker} Port"),
                    &value,
                    line.width as usize,
                    RowOverflow::Value,
                )
            }
            crate::state::ShareRow::Device(device_index) => {
                let path = state
                    .devices
                    .get(*device_index)
                    .map(|device| device.path.as_str())
                    .unwrap_or("");
                let checkbox = if state.draft_checked.contains(path) {
                    "[x]"
                } else {
                    "[ ]"
                };
                format!("{marker} {checkbox} {path}")
            }
            crate::state::ShareRow::StartStop => {
                let label = if state.sharing.active {
                    if state.share_confirm == Some(crate::state::ShareConfirm::Stop) {
                        "[ Confirm stop ]"
                    } else {
                        "[ Stop sharing ]"
                    }
                } else if state.has_unsaved_share() {
                    if state.share_confirm == Some(crate::state::ShareConfirm::Start) {
                        "[ Save and start ]"
                    } else {
                        "[ Start sharing ]"
                    }
                } else {
                    "[ Start sharing ]"
                };
                format!(
                    "{marker} {label}  {}",
                    if state.sharing.active {
                        "disconnects every remote client"
                    } else {
                        "listens only on the address above"
                    }
                )
            }
            crate::state::ShareRow::Save => {
                if state.share_confirm == Some(crate::state::ShareConfirm::Rebind) {
                    format!("{marker} [ Confirm change ]")
                } else {
                    format!("{marker} [ Save ]")
                }
            }
            crate::state::ShareRow::Cancel => format!("{marker} [ Cancel ]"),
            crate::state::ShareRow::CopyHuman => format!("{marker} [ Copy human key ]"),
            crate::state::ShareRow::CopyAi => format!("{marker} [ Copy MCP/AI key ]"),
            crate::state::ShareRow::ShowHuman => format!("{marker} [ Show human key ]"),
            crate::state::ShareRow::ShowAi => format!("{marker} [ Show MCP/AI key ]"),
            crate::state::ShareRow::Advanced => {
                if state.pairing_advanced {
                    format!("{marker} [ Hide older host fields ]")
                } else {
                    format!("{marker} [ Older host fields ]")
                }
            }
        };
        let style = if selected {
            highlighted(true)
        } else {
            RStyle::default()
        };
        put(buffer, line, 0, &text, style);
        if selected {
            cursor = Some((line.x + 1, line.y));
        }
    }
    if layout.details.height > 0 {
        if state.sharing.active && state.sharing.pairing.is_some() {
            let lines = pairing_lines(state);
            let width = layout.details.width.max(1) as usize;
            let wrapped: Vec<String> = lines
                .iter()
                .flat_map(|line| wrap_line(line, width))
                .collect();
            let height = layout.details.height as usize;
            let max_first = wrapped.len().saturating_sub(height);
            let first_detail = state.pairing_scroll.min(max_first);
            for row in 0..height {
                let Some(text) = wrapped.get(first_detail + row) else {
                    break;
                };
                put(
                    buffer,
                    Rect::new(
                        layout.details.x,
                        layout.details.y + row as u16,
                        layout.details.width,
                        1,
                    ),
                    0,
                    text,
                    RStyle::default(),
                );
            }
        } else {
            put(
                buffer,
                layout.details,
                0,
                "Check at least one local device, then Start sharing. Nothing listens until you do.",
                dim(),
            );
        }
    }
    let error_style = RStyle::default().fg(RColor::White).bg(RColor::Red);
    let mut error_shown = false;
    if let Some(error) = layout.error {
        let rebind_warning = state.share_confirm == Some(crate::state::ShareConfirm::Rebind);
        let text = state.share_error.as_deref().unwrap_or(if rebind_warning {
            "Remote clients disconnect; fresh long keys are issued."
        } else {
            ""
        });
        let style = if rebind_warning {
            RStyle::default().fg(RColor::Black).bg(RColor::Yellow)
        } else {
            error_style
        };
        put(buffer, error, 0, &format!(" {text} "), style);
        error_shown = true;
    } else if let Some(error) = &state.share_error {
        // A short page has no dedicated error row; the hint row carries the
        // failure instead of hiding it.
        if layout.hints.height > 0 {
            put(buffer, layout.hints, 0, &format!(" {error} "), error_style);
            error_shown = true;
        } else if area.height > 0 {
            put(
                buffer,
                Rect::new(area.x, area.y + area.height - 1, area.width, 1),
                0,
                &format!(" {error} "),
                error_style,
            );
            error_shown = true;
        }
    }
    if layout.hints.height > 0 && !error_shown {
        put(
            buffer,
            layout.hints,
            0,
            " ↑/↓ select · Enter edit or activate · Space toggle · Esc back ",
            dim(),
        );
    }
    cursor.or(Some((area.x + 1, area.y)))
}

/// Split one line into display-width-bounded pieces, never in the middle of a
/// wide character.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for ch in line.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > width && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += ch_width;
    }
    pieces.push(current);
    pieces
}

/// What a click in the Connect page landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectHit {
    Field(ConnectField),
    Connect,
    ClearKey,
    Cancel,
    ToggleMode,
}

const CONNECT_ACTIONS_ROW: &str = "[ Connect ]  [ Clear key ]  [ Cancel ]  [ Older fields ]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ConnectLayout {
    title: Rect,
    fields: Rect,
    bar: Rect,
    error: Option<Rect>,
    hints: Rect,
}

fn connect_layout(state: &AppState, right: Rect) -> Option<ConnectLayout> {
    if right.height < 1 || right.width < 12 {
        return None;
    }
    let hints_height = if right.height >= 6 { 1 } else { 0 };
    let title_height = if right.height >= 4 { 1 } else { 0 };
    // The action bar never clips a control: one row when it fits, then two,
    // then one full-width row per button on a genuinely narrow pane.
    let bar_height = if right.height >= 5 {
        let rows = if right.width >= CONNECT_ACTIONS_ROW.len() as u16 {
            1
        } else if right.width >= 38 {
            2
        } else {
            4
        };
        rows.min(right.height.saturating_sub(1))
    } else {
        0
    };
    let error_height = u16::from(
        state
            .connect
            .as_ref()
            .is_some_and(|form| form.error.is_some())
            && right.height >= 2,
    );
    let reserved = title_height + bar_height + hints_height + error_height;
    let fields_height = right.height.saturating_sub(reserved).max(1);
    let title = Rect::new(right.x, right.y, right.width, title_height);
    let fields = Rect::new(right.x, right.y + title_height, right.width, fields_height);
    let error = (error_height > 0).then(|| {
        Rect::new(
            right.x,
            right.y + title_height + fields_height,
            right.width,
            1,
        )
    });
    let bar_y = title_height + fields_height + error_height;
    let bar = Rect::new(right.x, right.y + bar_y, right.width, bar_height);
    let hints = Rect::new(
        right.x,
        right.y + right.height - hints_height,
        right.width,
        hints_height,
    );
    Some(ConnectLayout {
        title,
        fields,
        bar,
        error,
        hints,
    })
}

/// The first Connect field the pane shows, so the selected field stays in
/// view when the terminal is short.
fn connect_first_field(state: &AppState, visible: usize) -> usize {
    let Some(form) = state.connect.as_ref() else {
        return 0;
    };
    let fields = form.fields();
    let selected = fields
        .iter()
        .position(|field| *field == form.field)
        .unwrap_or(0);
    let visible = visible.max(1);
    selected
        .saturating_sub(visible - 1)
        .min(fields.len().saturating_sub(visible))
}

/// Hit test one click in the Connect page.
pub fn connect_hit_test(
    state: &AppState,
    right: Rect,
    column: u16,
    row: u16,
) -> Option<ConnectHit> {
    let form = state.connect.as_ref()?;
    let layout = connect_layout(state, right)?;
    if in_rect(layout.fields, column, row) {
        let first = connect_first_field(state, layout.fields.height as usize);
        let index = first + (row - layout.fields.y) as usize;
        return form.fields().get(index).copied().map(ConnectHit::Field);
    }
    if !in_rect(layout.bar, column, row) {
        return None;
    }
    // Offsets are the same as the drawn buttons, so a click always matches a
    // visible label.
    let offset = column.saturating_sub(layout.bar.x);
    if layout.bar.height >= 4 {
        return Some(match row.saturating_sub(layout.bar.y) {
            0 => ConnectHit::Connect,
            1 => ConnectHit::ClearKey,
            2 => ConnectHit::Cancel,
            _ => ConnectHit::ToggleMode,
        });
    }
    if layout.bar.height == 2 {
        if row == layout.bar.y + 1 {
            return Some(ConnectHit::ToggleMode);
        }
        return Some(if offset < 13 {
            ConnectHit::Connect
        } else if offset < 28 {
            ConnectHit::ClearKey
        } else {
            ConnectHit::Cancel
        });
    }
    if offset < 12 {
        Some(ConnectHit::Connect)
    } else if offset < 28 {
        Some(ConnectHit::ClearKey)
    } else if offset < 40 {
        Some(ConnectHit::Cancel)
    } else {
        Some(ConnectHit::ToggleMode)
    }
}

/// The remote-connect form: address, port and one sharing key in its normal
/// mode, or the older host fields in advanced mode. The key and credential are
/// masked; nothing here is written anywhere and no connection happens until
/// the explicit Connect button (or Enter) is used.
fn draw_connect(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let Some(form) = &state.connect else {
        put(buffer, area, 1, "Press c or use the Connect button.", dim());
        return Some((area.x + 1, area.y));
    };
    let Some(layout) = connect_layout(state, area) else {
        put(
            buffer,
            area,
            0,
            " Esc cancels · widen the pane to connect ",
            dim(),
        );
        return None;
    };
    if layout.title.height > 0 {
        let title = match form.mode {
            FormMode::Key => " Connect · sharing key ",
            FormMode::Advanced => " Connect · older host fields (Ctrl+A for a key) ",
        };
        let title_style = focus_title_style(state.focus == Focus::Right);
        put(buffer, layout.title, 0, title, title_style);
    }
    let fields = form.fields();
    let mut cursor = None;
    let first = connect_first_field(state, layout.fields.height as usize);
    for (offset, field) in fields
        .iter()
        .enumerate()
        .skip(first)
        .take(layout.fields.height as usize)
    {
        let selected = *field == form.field;
        let marker = if selected { "▸" } else { " " };
        let line = Rect::new(
            layout.fields.x,
            layout.fields.y + offset.saturating_sub(first) as u16,
            layout.fields.width,
            1,
        );
        let text = row_text(
            &format!("{marker} {}", field.name()),
            &form.value(*field),
            line.width as usize,
            RowOverflow::Tail,
        );
        put(buffer, line, 0, &text, highlighted(selected));
        if selected {
            let column = display_width(&text).min(line.width).saturating_sub(1);
            cursor = Some((line.x + column, line.y));
        }
    }
    if layout.bar.height > 0 {
        let rows: &[&str] = match layout.bar.height {
            1 => &[CONNECT_ACTIONS_ROW],
            2 => &["[ Connect ]  [ Clear key ]  [ Cancel ]", "[ Older fields ]"],
            _ => &[
                "[ Connect ]",
                "[ Clear key ]",
                "[ Cancel ]",
                "[ Older fields ]",
            ],
        };
        for (offset, text) in rows.iter().enumerate() {
            put(
                buffer,
                Rect::new(
                    layout.bar.x,
                    layout.bar.y + offset as u16,
                    layout.bar.width,
                    1,
                ),
                0,
                text,
                dim(),
            );
        }
    }
    if let Some(error) = layout.error {
        put(
            buffer,
            error,
            0,
            &format!(" {} ", form.error.as_deref().unwrap_or_default()),
            RStyle::default().fg(RColor::White).bg(RColor::Red),
        );
    }
    if layout.hints.height > 0 {
        put(
            buffer,
            layout.hints,
            0,
            " Tab/↑↓ field · Enter connects · Ctrl+L or Delete clears the key · Ctrl+A older · Esc cancels ",
            dim(),
        );
    }
    cursor.or(Some((area.x + 1, area.y)))
}

/// The MCP/AI pairing form: address, port and one AI sharing key, saved to the
/// owner-only pairing file. The key is masked and never dialled from here.
fn draw_mcp_pair(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let Some(form) = &state.mcp_pair else {
        put(
            buffer,
            area,
            1,
            "Press m in the sidebar to configure an MCP pairing.",
            dim(),
        );
        return Some((area.x + 1, area.y));
    };
    let fields: Vec<(McpField, &'static str, String)> = McpField::ALL
        .iter()
        .map(|field| (*field, field.name(), form.value(*field)))
        .collect();
    draw_form(
        state,
        buffer,
        area,
        " Configure MCP pairing · saved to mcp-peers.json ",
        &fields,
        form.field,
        form.error.as_deref(),
    )
}

/// Draw one labelled form, keeping the active field visible at every size.
fn draw_form<F: PartialEq + Copy>(
    state: &AppState,
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    fields: &[(F, &'static str, String)],
    selected_field: F,
    error: Option<&str>,
) -> Option<(u16, u16)> {
    let available = area.height as usize;
    if available == 0 || area.width == 0 {
        return None;
    }
    let show_error = error.is_some() && available >= 2;
    // Keep the active field visible at every size. The title yields first,
    // then the error row; at least one row is reserved for editable content.
    let show_title = available >= 2 + usize::from(show_error);
    let title_rows = usize::from(show_title);
    if show_title {
        let title_style = focus_title_style(state.focus == Focus::Right);
        put(buffer, area, 0, title, title_style);
    }
    let error_rows = usize::from(show_error);
    let visible = available.saturating_sub(title_rows + error_rows).max(1);
    let mut cursor = None;
    let selected_index = fields
        .iter()
        .position(|(field, _, _)| *field == selected_field)
        .unwrap_or(0);
    let first_visible = selected_index
        .saturating_sub(visible - 1)
        .min(fields.len().saturating_sub(visible));
    for (offset, (field, label, value)) in
        fields.iter().enumerate().skip(first_visible).take(visible)
    {
        let selected = *field == selected_field;
        let row_offset = offset - first_visible + title_rows;
        let marker = if selected { "▸" } else { " " };
        let line = Rect::new(area.x, area.y + row_offset as u16, area.width, 1);
        let width = area.width as usize;
        let full_label = format!("{marker} {label}");
        let text = row_text(&full_label, value, width, RowOverflow::Tail);
        put(buffer, line, 0, &text, highlighted(selected));
        if selected {
            let column = display_width(&text).min(width as u16).saturating_sub(1);
            cursor = Some((area.x + column, line.y));
        }
    }
    if show_error {
        if let Some(error) = error {
            put(
                buffer,
                Rect::new(
                    area.x,
                    area.y + area.height.saturating_sub(1),
                    area.width,
                    1,
                ),
                0,
                &format!(" {error} "),
                RStyle::default().fg(RColor::White).bg(RColor::Red),
            );
        }
    }
    cursor
}

fn draw_stream(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    if area.height == 0 || area.width == 0 {
        return None;
    }
    let Some(session) = state.shown_session() else {
        put(
            buffer,
            area,
            1,
            "Highlight a device on the left and press Enter to open or join it.",
            dim(),
        );
        return Some((area.x + 1, area.y));
    };
    let scroll = session.screen.scroll();
    for row in 0..area.height as usize {
        let y = area.y + row as u16;
        for col in 0..area.width as usize {
            let cell = session.screen.view_cell(row, col);
            let symbol = cell.ch.to_string();
            put_cell(
                buffer,
                area.x + col as u16,
                y,
                &symbol,
                screen_style(cell.style),
            );
        }
    }
    // The device's cursor belongs to the live screen: not in a scrolled-back
    // view, and not in the scrollback view, whose keys do not reach the device.
    if scroll > 0 || state.view == View::Scrollback {
        return None;
    }
    let (row, col) = session.screen.cursor();
    // When the sidebar has the hardware cursor, the stream still shows where
    // the device's live cursor is with a reversed cell.
    if state.focus != Focus::Right {
        let visible_row = row.min(area.height.saturating_sub(1) as usize);
        let visible_col = col.min(area.width.saturating_sub(1) as usize);
        let cell = session.screen.view_cell(visible_row, visible_col);
        // Reverse the cell so it stands out; on device reverse video, remove
        // the reverse instead, or the cursor would be invisible there.
        let mut style = screen_style(cell.style);
        style = if cell.style.reverse {
            style.remove_modifier(Modifier::REVERSED)
        } else {
            style.add_modifier(Modifier::REVERSED)
        };
        put_cell(
            buffer,
            area.x + visible_col as u16,
            area.y + visible_row as u16,
            &cell.ch.to_string(),
            style,
        );
    }
    Some((
        area.x + col.min(area.width.saturating_sub(1) as usize) as u16,
        area.y + row.min(area.height.saturating_sub(1) as usize) as u16,
    ))
}

fn draw_footer(state: &AppState, buffer: &mut Buffer, area: Rect) {
    if area.height == 0 {
        return;
    }
    let status = Rect::new(area.x, area.y, area.width, 1);
    let hints = (area.height > 1).then(|| Rect::new(area.x, area.y + 1, area.width, 1));
    let status_drawn = draw_status(state, buffer, status);
    match hints {
        Some(hints) => draw_hints(state, buffer, hints),
        None if !status_drawn => draw_hints(state, buffer, status),
        None => {}
    }
}

/// Draw the baud prompt or the latest notice. Returns whether the row said
/// anything of its own.
fn draw_status(state: &AppState, buffer: &mut Buffer, area: Rect) -> bool {
    if let Some(prompt) = &state.prompt {
        let text = format!(
            " Baud for {} [{}]: {}{}",
            prompt.path,
            serial_nexus_protocol::DEFAULT_BAUD,
            prompt.input,
            if prompt.error.is_some() { "" } else { "_" }
        );
        let style = RStyle::default()
            .fg(RColor::Yellow)
            .add_modifier(Modifier::BOLD);
        put(buffer, area, 0, &text, style);
        if let Some(error) = &prompt.error {
            let used = text.chars().count() as u16 + 1;
            put(
                buffer,
                area,
                used,
                &format!(" {error} "),
                RStyle::default().fg(RColor::White).bg(RColor::Red),
            );
        }
        return true;
    }
    if let Some(notice) = &state.notice {
        let style = match notice.severity {
            Severity::Error => RStyle::default().fg(RColor::White).bg(RColor::Red),
            Severity::Info => RStyle::default().fg(RColor::Yellow),
        };
        let text = match state.warning_position() {
            Some((position, total)) if total > 1 => {
                format!(
                    " warning {position}/{total}: {} — press x in the sidebar to dismiss ",
                    notice.text
                )
            }
            Some(_) => format!(
                " warning: {} — press x in the sidebar to dismiss ",
                notice.text
            ),
            None => format!(" {} ", notice.text),
        };
        put(buffer, area, 0, &text, style);
        return true;
    }
    if state.view == View::Scrollback {
        // Say where the view is: the footer's hints stay the same.
        let lines = state
            .shown_session()
            .map(|session| session.screen.scroll())
            .unwrap_or(0);
        let text = if lines == 0 {
            " At the live screen · ↑ or PageUp shows older output ".to_string()
        } else {
            format!(" Scrolled back {lines} lines ")
        };
        put(
            buffer,
            area,
            0,
            &text,
            RStyle::default()
                .fg(RColor::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        return true;
    }
    false
}

fn draw_hints(state: &AppState, buffer: &mut Buffer, area: Rect) {
    let hints: String = match (state.focus, state.view) {
        (Focus::Sidebar, _) => {
            " ↑/↓ · Enter/dbl-click open · b scroll · d disconnect · m MCP pair · Ctrl+] right · Ctrl+Q quit "
                .to_string()
        }
        (Focus::Right, View::Stream) => {
            // The stream forwards every ordinary key, so Ctrl+] is the way out.
            " Typing goes to the shown device (Tab and Ctrl+Q too) · Ctrl+] sidebar ".to_string()
        }
        // The scrollback view's keys navigate; none of them reach the device.
        (Focus::Right, View::Scrollback) => {
            " ↑/↓ line · PageUp/PageDown page · Esc back to live · Ctrl+] sidebar ".to_string()
        }
        (Focus::Right, View::Settings) => {
            " Ctrl+S save · Esc cancel · Tab/↑↓ move · Space/Enter toggle or edit ".to_string()
        }
        (Focus::Right, View::Share) => {
            " ↑/↓ select · Enter edit or activate · Space toggle · Esc back ".to_string()
        }
        (Focus::Right, View::Connect) => {
            " Tab/↑↓ field · Ctrl+A older-host fields · Enter connects · Esc cancels ".to_string()
        }
        (Focus::Right, View::McpPair) => {
            " Tab/↑↓ field · Enter saves to the pairing file · Esc cancels ".to_string()
        }
    };
    put(buffer, area, 0, &hints, dim());
}

fn draw_sidebar_button(
    buffer: &mut Buffer,
    area: Rect,
    selected: bool,
    label: &str,
    hint: &str,
) -> Option<(u16, u16)> {
    let marker = if selected { "›" } else { " " };
    let style = if selected {
        highlighted(true)
    } else {
        RStyle::default().fg(RColor::Cyan)
    };
    draw_row(buffer, area, &format!("{marker} {label}"), hint, style);
    selected.then_some((area.x + 1, area.y))
}

/// One sidebar or settings row: a label on the left, a value kept at the
/// right edge, and the selection style.
fn draw_row(buffer: &mut Buffer, area: Rect, label: &str, value: &str, style: RStyle) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let text = row_text(label, value, area.width as usize, RowOverflow::Label);
    put(buffer, area, 0, &text, style);
}

fn draw_value_row(buffer: &mut Buffer, area: Rect, label: &str, value: &str, style: RStyle) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let text = row_text(label, value, area.width as usize, RowOverflow::Value);
    put(buffer, area, 0, &text, style);
}

#[derive(Clone, Copy)]
enum RowOverflow {
    Label,
    Value,
    Tail,
}

fn row_text(label: &str, value: &str, width: usize, overflow: RowOverflow) -> String {
    if value.is_empty() {
        return fit(label, width);
    }
    let label_width = display_width(label) as usize;
    let value_width = display_width(value) as usize;
    if label_width + value_width < width {
        if matches!(overflow, RowOverflow::Tail) {
            return fit(&format!("{label} {value}"), width);
        }
        let mut line = label.to_string();
        line.push_str(&" ".repeat(width - label_width - value_width));
        line.push_str(value);
        return fit(&line, width);
    }
    match overflow {
        RowOverflow::Label => {
            // Preserve the label and clip the value when both cannot fit.
            let keep = width.saturating_sub(label_width + 1);
            if keep < 3 || label_width >= width {
                return fit(label, width);
            }
            let mut line = label.to_string();
            line.push(' ');
            line.push_str(fit(value, keep).trim_end());
            fit(&line, width)
        }
        RowOverflow::Value => {
            // Preserve a short value in settings rows, abbreviating the label.
            let keep = width.saturating_sub(value_width + 1);
            let mut line = fit(label, keep).trim_end().to_string();
            while display_width(&line) as usize > keep {
                line.pop();
            }
            line.push_str(&" ".repeat(keep.saturating_sub(display_width(&line) as usize)));
            line.push(' ');
            line.push_str(value);
            fit(&line, width)
        }
        RowOverflow::Tail => {
            // Keep the label and the most recently entered value characters.
            let room = width.saturating_sub(label_width + 1).max(1);
            let value_room = if label_width + 1 + room > width {
                width / 2
            } else {
                room
            };
            let tail: String = value
                .chars()
                .rev()
                .take(value_room)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let label_room = width.saturating_sub(display_width(&tail) as usize + 1);
            fit(&format!("{} {tail}", fit(label, label_room)), width)
        }
    }
}

fn focus_title_style(focused: bool) -> RStyle {
    if focused {
        RStyle::default()
            .fg(RColor::Black)
            .bg(RColor::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        RStyle::default()
            .fg(RColor::Cyan)
            .add_modifier(Modifier::BOLD)
    }
}

fn highlighted(selected: bool) -> RStyle {
    if selected {
        RStyle::default()
            .fg(RColor::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        RStyle::default()
    }
}

fn put(buffer: &mut Buffer, area: Rect, offset: u16, text: &str, style: RStyle) {
    let x = area.x + offset;
    let width = area.width.saturating_sub(offset);
    if area.height == 0 || width == 0 || x >= buffer.area.right() || area.y >= buffer.area.bottom()
    {
        return;
    }
    let width = width.min(buffer.area.right().saturating_sub(x));
    buffer.set_stringn(x, area.y, text, width as usize, style);
}

fn put_cell(buffer: &mut Buffer, x: u16, y: u16, symbol: &str, style: RStyle) {
    if x >= buffer.area.right() || y >= buffer.area.bottom() {
        return;
    }
    let cell = &mut buffer[(x, y)];
    cell.set_symbol(symbol);
    cell.set_style(style);
}

fn dim() -> RStyle {
    RStyle::default().add_modifier(Modifier::DIM)
}

/// One line as exactly `width` display columns: clipped if longer, padded if
/// shorter. Wide characters are never split down the middle.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cell = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cell > width {
            break;
        }
        out.push(ch);
        used += cell;
    }
    while used < width {
        out.push(' ');
        used += 1;
    }
    out
}

fn display_width(text: &str) -> u16 {
    UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
}

fn screen_style(style: Style) -> RStyle {
    let mut out = RStyle::default();
    if let Some(fg) = style.fg {
        out = out.fg(to_ratatui(fg));
    }
    if let Some(bg) = style.bg {
        out = out.bg(to_ratatui(bg));
    }
    let mut modifier = Modifier::empty();
    if style.bold {
        modifier |= Modifier::BOLD;
    }
    if style.dim {
        modifier |= Modifier::DIM;
    }
    if style.underline {
        modifier |= Modifier::UNDERLINED;
    }
    if style.reverse {
        modifier |= Modifier::REVERSED;
    }
    out.add_modifier(modifier)
}

fn to_ratatui(colour: Color) -> RColor {
    match colour {
        Color::Default => RColor::Reset,
        Color::Indexed(index) => RColor::Indexed(index),
        Color::Rgb(red, green, blue) => RColor::Rgb(red, green, blue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionKey;
    use crate::state::{AppState, ConnectForm, DeviceEntry, EndpointId, Focus, View};
    use serial_nexus_protocol::Event;

    fn screen(state: &AppState, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        render(state, &mut buffer, area);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn demo() -> AppState {
        let mut state = AppState::new(12, 40);
        let mut first = DeviceEntry::new("/dev/ttyUSB0");
        first.description = Some("FTDI adapter".into());
        let mut second = DeviceEntry::new("/dev/ttyUSB1");
        second.open = true;
        second.device_id = Some(7);
        second.baud = Some(57_600);
        state.set_devices(vec![first, second]);
        state.add_session(7, "/dev/ttyUSB1", 57_600, None);
        state
    }

    #[test]
    fn a_usable_width_shows_the_sidebar_beside_the_stream() {
        let mut state = demo();
        state.apply_event(&Event::device_output(7, b"ready\r\n"));
        let area = Rect::new(0, 0, 100, 16);
        let panes = panes(area);
        assert!(panes.split, "100 columns fits both panes");
        assert_eq!(panes.sidebar.height, panes.right.height);
        assert_eq!(panes.right.x, panes.sidebar.right() + 1);

        let mut buffer = Buffer::empty(area);
        render(&state, &mut buffer, area);
        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("Devices (2)"), "{text}");
        assert!(text.contains("/dev/ttyUSB0"), "{text}");
        assert!(text.contains("57600"), "the baud stays visible: {text}");
        assert!(text.contains("[ Settings ]"), "{text}");
        assert!(text.contains("ready"), "the stream is beside it: {text}");
        // The stream starts in the right pane, not at the screen edge.
        assert_eq!(buffer[(panes.right.x, panes.right.y)].symbol(), "r");
    }

    #[test]
    fn an_eighty_column_sidebar_still_shows_a_devices_status() {
        let state = demo();
        let text = screen(&state, 80, 16).join("\n");
        let row = text
            .lines()
            .find(|line| line.contains("● /dev/ttyUSB1"))
            .expect("shown row");
        assert!(row.contains("57600"), "the baud is visible: {row:?}");
    }

    #[test]
    fn a_narrow_terminal_shows_only_the_focused_pane() {
        let mut state = demo();
        let area = Rect::new(0, 0, 50, 16);
        let panes = panes(area);
        assert!(!panes.split, "50 columns is too narrow for both panes");

        // Sidebar focused: no stream on screen, the device list is.
        let text = screen(&state, 50, 16).join("\n");
        assert!(text.contains("Devices (2)"), "{text}");
        assert!(!text.contains("ready"), "{text}");

        // Right focused: the stream fills the pane, no sidebar.
        state.focus = Focus::Right;
        state.apply_event(&Event::device_output(7, b"ready\r\n"));
        let text = screen(&state, 50, 16).join("\n");
        assert!(text.contains("ready"), "{text}");
        assert!(!text.contains("Devices ("), "{text}");
    }

    #[test]
    fn the_highlighted_and_shown_rows_are_marked_differently() {
        let mut state = demo();
        state.select_index(0);
        let lines = screen(&state, 100, 16);
        let first = lines
            .iter()
            .find(|line| line.contains("○ /dev/ttyUSB0"))
            .expect("first row");
        let second = lines
            .iter()
            .find(|line| line.contains("● /dev/ttyUSB1"))
            .expect("shown row");
        assert!(first.contains('›'), "highlighted, not shown: {first:?}");
        assert!(second.contains('▹'), "shown, not highlighted: {second:?}");

        // The shown row keeps a legible marker even when it is also
        // highlighted.
        state.select_path("/dev/ttyUSB1");
        let lines = screen(&state, 100, 16);
        let second = lines
            .iter()
            .find(|line| line.contains("● /dev/ttyUSB1"))
            .expect("shown row");
        assert!(second.contains('▸'), "{second:?}");
    }

    #[test]
    fn the_header_names_the_shown_device_and_the_focused_pane() {
        let mut state = demo();
        state
            .sessions
            .get_mut(&SessionKey::local(7))
            .unwrap()
            .control = Some("claude".into());
        let header = &screen(&state, 100, 16)[0];
        assert!(header.contains("/dev/ttyUSB1"), "{header}");
        assert!(header.contains("57600 baud"), "{header}");
        assert!(header.contains("AI control: claude"), "{header}");
        assert!(header.contains("attached"), "{header}");
        assert!(header.contains("focus: sidebar"), "{header}");

        state.focus = Focus::Right;
        assert!(screen(&state, 100, 16)[0].contains("focus: stream"));
    }

    #[test]
    fn the_cursor_sits_on_the_device_cursor_in_the_focused_stream() {
        let mut state = demo();
        state.focus = Focus::Right;
        state.apply_event(&Event::device_output(7, b"boot> "));
        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area);
        let right = panes(area).right;
        assert_eq!(cursor, Some((right.x + 6, right.y)), "six columns along");
    }

    #[test]
    fn the_live_cursor_is_visible_even_when_the_sidebar_is_focused() {
        let mut state = demo();
        state.apply_event(&Event::device_output(7, b"boot> "));
        assert_eq!(state.focus, Focus::Sidebar);
        let area = Rect::new(0, 0, 100, 16);
        let right = panes(area).right;
        let mut buffer = Buffer::empty(area);
        render(&state, &mut buffer, area);
        let cell = &buffer[(right.x + 6, right.y)];
        assert_eq!(cell.symbol(), " ");
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "the live cursor is drawn as a reversed cell: {cell:?}"
        );
    }

    #[test]
    fn the_live_cursor_stays_visible_on_device_reverse_video() {
        let mut state = demo();
        state.apply_event(&Event::device_output(7, b"\x1b[7mA\x1b[D"));
        assert_eq!(state.focus, Focus::Sidebar);
        let area = Rect::new(0, 0, 100, 16);
        let right = panes(area).right;
        let mut buffer = Buffer::empty(area);
        render(&state, &mut buffer, area);
        let cell = &buffer[(right.x, right.y)];
        assert_eq!(cell.symbol(), "A");
        assert!(
            !cell.modifier.contains(Modifier::REVERSED),
            "reverse is removed so the cursor is not invisible: {cell:?}"
        );
    }

    #[test]
    fn compact_connect_layout_keeps_the_selected_field_visible() {
        let mut state = demo();
        let mut form = ConnectForm::new();
        form.address = "fe80::1%3".into();
        state.connect = Some(form);
        state.view = View::Connect;
        state.focus = Focus::Right;

        let text = screen(&state, 80, 3).join("\\n");
        assert!(text.contains("Host address"), "{text}");
        assert!(text.contains("fe80::1%3"), "{text}");
    }

    #[test]
    fn compact_connect_error_does_not_overwrite_the_selected_field() {
        let mut state = demo();
        let mut form = ConnectForm::new();
        form.address = "host.invalid".into();
        form.error = Some("the host could not be reached".into());
        state.connect = Some(form);
        state.view = View::Connect;
        state.focus = Focus::Right;

        let text = screen(&state, 80, 4).join("\\n");
        assert!(text.contains("Host address"), "{text}");
        assert!(text.contains("host.invalid"), "{text}");
        assert!(text.contains("could not be reached"), "{text}");
    }

    #[test]
    fn a_narrow_settings_view_keeps_the_value_visible() {
        let mut state = demo();
        state.begin_settings();
        state.draft.local_echo = false;
        let area = Rect::new(0, 0, 13, 10);
        let text = screen(&state, 13, 10).join("\n");
        assert!(text.contains("off"), "{text}");
        assert!(settings_layout(panes(area).right).is_some());
    }

    #[test]
    fn the_cursor_sits_on_the_highlighted_sidebar_row_and_the_button() {
        let mut state = demo();
        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let sidebar = sidebar(panes(area).sidebar).unwrap();
        let rows = state.sidebar_rows();
        let first = first_sidebar_row(&state, &rows, sidebar);
        let selected_row = rows
            .iter()
            .position(|row| matches!(row, SidebarRow::Device(index) if *index == state.selected))
            .unwrap_or(0);
        assert_eq!(
            cursor,
            (
                sidebar.list.x + 1,
                sidebar.list.y + (selected_row - first) as u16
            )
        );

        state.select_settings_button();
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let button = settings_button(area).expect("button");
        assert_eq!(cursor, (button.x + 1, button.y));
    }

    #[test]
    fn an_empty_connected_host_stays_visible_in_a_full_sidebar() {
        let mut state = AppState::new(16, 40);
        state.set_devices(
            (0..8)
                .map(|index| DeviceEntry::new(format!("/dev/tty{index}")))
                .collect(),
        );
        state
            .remote_labels
            .insert(EndpointId(1), "Remote 192.0.2.10:7331".into());
        state.select_index(0);

        let rendered = screen(&state, 80, 16).join("\\n");
        assert!(rendered.contains("Remote 192.0.2.10:7331"), "{rendered}");
        assert!(rendered.contains("/dev/tty0"), "{rendered}");
    }

    #[test]
    fn scrolled_sidebar_clicks_and_cursor_match_drawn_device_rows() {
        let mut state = AppState::new(12, 40);
        state.set_devices(
            (0..20)
                .map(|index| DeviceEntry::new(format!("/dev/tty{index:02}")))
                .collect(),
        );
        state
            .remote_labels
            .insert(EndpointId(1), "Remote 192.0.2.10:7331".into());
        state.select_index(15);
        let area = Rect::new(0, 0, 80, 12);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("selected device cursor");
        assert!(
            screen(&state, 80, 12)
                .join("\\n")
                .contains("Remote 192.0.2.10:7331"),
            "the empty host keeps its own endpoint group"
        );
        let lines = screen(&state, 80, 12);
        let drawn_row = lines
            .iter()
            .position(|line| line.contains("/dev/tty15"))
            .expect("selected device is drawn");
        assert_eq!(cursor.1 as usize, drawn_row);
        let sidebar = sidebar(panes(area).sidebar).expect("sidebar");
        assert_eq!(
            hit_test(&state, area, sidebar.list.x + 1, drawn_row as u16),
            Some(Hit::Device(15))
        );
    }

    #[test]
    fn a_short_sidebar_never_hides_the_selected_device_for_a_host_heading() {
        let mut state = AppState::new(8, 80);
        state.set_devices(vec![DeviceEntry::new("/dev/local")]);
        state
            .remote_labels
            .insert(EndpointId(1), "empty host".into());
        let area = Rect::new(0, 0, 80, 8);
        let layout = sidebar(panes(area).sidebar).expect("sidebar");
        let rows = state.sidebar_rows();
        let first = first_sidebar_row(&state, &rows, layout);
        let selected = rows
            .iter()
            .position(|row| *row == SidebarRow::Device(0))
            .expect("selected device row");

        assert!(selected >= first);
        assert!(selected < first + layout.list.height as usize);
        assert!(screen(&state, 80, 8).join("\\n").contains("Empty host: #1"));
    }

    #[test]
    fn many_empty_hosts_cannot_consume_every_short_sidebar_device_row() {
        let mut state = AppState::new(12, 100);
        state.set_endpoint_devices(LOCAL_ENDPOINT, vec![DeviceEntry::new("/dev/local")]);
        let remote = EndpointId(20);
        state.set_endpoint_devices(remote, vec![DeviceEntry::on(remote, "/dev/remote")]);
        for id in 1..=5 {
            state
                .remote_labels
                .insert(EndpointId(id), format!("empty host {id}"));
        }
        state.remote_labels.insert(remote, "device host".into());
        state.select_index(
            state
                .devices
                .iter()
                .position(|device| device.path == "/dev/remote")
                .expect("remote device"),
        );
        let area = Rect::new(0, 0, 100, 12);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("selected device cursor");
        let lines = screen(&state, 100, 12);
        let drawn_row = lines
            .iter()
            .position(|line| line.contains("/dev/remote"))
            .expect("remote device remains visible");
        let sidebar = sidebar(panes(area).sidebar).expect("sidebar");

        assert_eq!(cursor.1 as usize, drawn_row);
        assert_eq!(
            hit_test(&state, area, sidebar.list.x + 1, drawn_row as u16),
            Some(Hit::Device(state.selected))
        );
    }

    #[test]
    fn a_zero_list_sidebar_still_names_an_empty_connected_host() {
        let mut state = AppState::new(7, 80);
        state.set_devices(vec![DeviceEntry::new("/dev/local")]);
        state
            .remote_labels
            .insert(EndpointId(1), "empty host".into());

        let rendered = screen(&state, 80, 7).join("\\n");
        assert!(rendered.contains("Empty host: #1"), "{rendered}");
        assert!(rendered.contains("/dev/local"), "{rendered}");
    }

    #[test]
    fn a_two_row_sidebar_keeps_the_settings_button_reachable() {
        let sidebar = sidebar(Rect::new(0, 0, 20, 2)).expect("compact sidebar");
        assert_eq!(sidebar.button, Rect::new(0, 1, 20, 1));
        assert_eq!(sidebar.share.height, 0);
        assert_eq!(sidebar.connect.height, 0);
        assert_eq!(sidebar.list.height, 0);
    }

    #[test]
    fn clicks_find_device_rows_and_the_settings_button_beside_the_stream() {
        let state = demo();
        let area = Rect::new(0, 0, 100, 16);
        let sidebar = sidebar(panes(area).sidebar).unwrap();
        assert_eq!(
            hit_test(&state, area, sidebar.list.x, sidebar.list.y),
            Some(Hit::Device(0))
        );
        let offset = (state.devices.len() - 1) as u16;
        assert_eq!(
            hit_test(&state, area, sidebar.list.x, sidebar.list.y + offset),
            Some(Hit::Device(1))
        );
        assert_eq!(
            hit_test(&state, area, sidebar.button.x, sidebar.button.y),
            Some(Hit::Settings)
        );
        assert_eq!(
            hit_test(&state, area, 0, 0),
            None,
            "the header is not a control"
        );
        assert_eq!(
            hit_test(&state, area, panes(area).right.x, panes(area).right.y),
            None,
            "the right pane is not a sidebar control"
        );
    }

    #[test]
    fn the_settings_view_shows_both_flags_and_the_cursor_follows_the_selection() {
        let mut state = demo();
        state.begin_settings();
        state.draft.local_echo = false;
        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("Settings"), "{text}");
        assert!(text.contains("Local echo"), "{text}");
        assert!(text.contains("Scrollback"), "{text}");
        assert!(
            text.contains("Devices (2)"),
            "the sidebar stays visible while Settings is open: {text}"
        );
        let row = text
            .lines()
            .find(|line| line.contains("Local echo"))
            .expect("local echo row");
        assert!(row.contains("off"), "{row}");
        let scrollback = text
            .lines()
            .find(|line| line.contains("Scrollback"))
            .expect("scrollback row");
        assert!(scrollback.contains("on"), "{scrollback}");
        assert!(text.contains("Ctrl+S save"), "{text}");
        assert!(
            text.contains("[ Save ]"),
            "a clickable save is drawn: {text}"
        );
        assert!(text.contains("[ Cancel ]"), "{text}");

        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let rows = settings_layout(panes(area).right).unwrap().rows;
        assert_eq!(cursor.1, rows.y);

        state.settings_row = 1;
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        assert_eq!(cursor.1, rows.y + 1);
    }

    #[test]
    fn settings_rebind_confirmation_explains_disconnect_and_key_rotation() {
        let mut state = demo();
        state.begin_settings();
        state.settings_rebind_confirm = true;

        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("Confirm endpoint change"), "{text}");
        assert!(text.contains("[ Confirm ]"), "{text}");
        assert!(
            text.contains("Remotes disconnect; fresh long keys"),
            "{text}"
        );
    }

    #[test]
    fn clicks_find_the_flags_and_the_save_and_cancel_buttons() {
        let mut state = demo();
        state.begin_settings();
        let area = Rect::new(0, 0, 100, 16);
        let right = panes(area).right;
        let layout = settings_layout(right).unwrap();
        assert_eq!(
            settings_hit_test(&state, right, layout.save.x, layout.save.y),
            Some(SettingsHit::Save)
        );
        assert_eq!(
            settings_hit_test(&state, right, layout.cancel.x, layout.cancel.y),
            Some(SettingsHit::Cancel)
        );
        assert_eq!(
            settings_hit_test(&state, right, layout.rows.x, layout.rows.y),
            Some(SettingsHit::Flag(0))
        );
        assert_eq!(
            settings_hit_test(&state, right, layout.rows.x, layout.rows.y + 1),
            Some(SettingsHit::Flag(1))
        );
        assert_eq!(settings_hit_test(&state, right, 0, 0), None);

        // A very short terminal still draws the buttons when the area allows.
        let small = Rect::new(0, 0, 40, 7);
        let layout = settings_layout(panes(small).right).unwrap();
        assert!(
            settings_hit_test(&state, panes(small).right, layout.save.x, layout.save.y).is_some()
        );
    }

    #[test]
    fn a_short_settings_view_still_shows_both_flags() {
        let mut state = demo();
        state.begin_settings();
        state.settings_row = 1;
        let lines = screen(&state, 80, 7);
        let text = lines.join("\n");
        assert!(text.contains("Local echo"), "{text}");
        assert!(text.contains("Scrollback"), "{text}");
        let area = Rect::new(0, 0, 80, 7);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let rows = settings_layout(panes(area).right).unwrap().rows;
        assert!(
            cursor.1 >= rows.y && cursor.1 < rows.y + rows.height,
            "the cursor stays on a flag row, not the hints: {cursor:?}"
        );
        assert!(
            lines[cursor.1 as usize].contains("Scrollback"),
            "the cursor row is the selected flag: {:?}",
            lines[cursor.1 as usize]
        );
    }

    #[test]
    fn a_short_sidebar_shows_the_highlighted_device_under_the_cursor() {
        let state = demo();
        let area = Rect::new(0, 0, 80, 7);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let row: String = (0..area.width)
            .map(|x| buffer[(x, cursor.1)].symbol())
            .collect();
        assert!(
            row.contains(&state.devices[state.selected].path),
            "the cursor row names the highlighted device: {row:?}"
        );
        assert!(!row.contains("[ Settings ]"), "{row:?}");
    }

    #[test]
    fn a_four_row_settings_view_keeps_the_cursor_on_the_selected_flag() {
        let mut state = demo();
        state.begin_settings();
        state.settings_row = 1;
        let area = Rect::new(0, 0, 80, 4);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let rows = settings_layout(panes(area).right).unwrap().rows;
        assert!(
            cursor.1 >= rows.y && cursor.1 < rows.y + rows.height,
            "the cursor is on a flag row, not the footer: {cursor:?}"
        );
        let row_text: String = (0..area.width)
            .map(|x| buffer[(x, cursor.1)].symbol())
            .collect();
        assert!(row_text.contains("Scrollback"), "{row_text:?}");
    }

    #[test]
    fn the_scrollback_view_draws_the_kept_rows_in_the_right_pane() {
        let mut state = demo();
        state.focus = Focus::Right;
        state.view = View::Scrollback;
        state.resize(4, 40);
        for index in 0..8u8 {
            let line = format!("line{index}\r\n");
            state.apply_event(&Event::device_output(7, line.as_bytes()));
        }
        let live = screen(&state, 100, 7).join("\n");
        assert!(live.contains("line7"), "{live}");
        assert!(live.contains("At the live screen"), "{live}");
        assert!(
            live.contains("[ Share: off ]"),
            "the sidebar is still beside the scrollback: {live}"
        );

        let history = state
            .sessions
            .get(&SessionKey::local(7))
            .unwrap()
            .screen
            .history_len();
        assert!(history > 0, "the rows above the live view were kept");
        state
            .sessions
            .get_mut(&SessionKey::local(7))
            .unwrap()
            .screen
            .scroll_back(history);

        let scrolled = screen(&state, 100, 7).join("\n");
        assert!(scrolled.contains("line0"), "{scrolled}");
        assert!(!scrolled.contains("line7"), "{scrolled}");
        assert!(scrolled.contains("Scrolled back"), "{scrolled}");
    }

    #[test]
    fn the_cursor_is_hidden_in_the_scrollback_view() {
        let mut state = demo();
        state.focus = Focus::Right;
        state.resize(4, 40);
        state.apply_event(&Event::device_output(
            7,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n",
        ));
        let area = Rect::new(0, 0, 100, 7);
        let mut buffer = Buffer::empty(area);
        assert!(render(&state, &mut buffer, area).is_some(), "live cursor");

        state.view = View::Scrollback;
        assert_eq!(
            render(&state, &mut buffer, area),
            None,
            "the scrollback view has no device cursor"
        );
    }

    #[test]
    fn the_scrollback_hints_name_its_keys_and_the_stream_keeps_its_own() {
        let mut state = demo();
        state.focus = Focus::Right;
        let stream = screen(&state, 100, 16).join("\n");
        assert!(stream.contains("Ctrl+] sidebar"), "{stream}");
        assert!(!stream.contains("Shift+PageUp"), "{stream}");

        state.view = View::Scrollback;
        let scrollback = screen(&state, 100, 16).join("\n");
        assert!(scrollback.contains("PageUp/PageDown"), "{scrollback}");
        assert!(scrollback.contains("Esc back to live"), "{scrollback}");
    }

    #[test]
    fn both_the_scroll_and_the_way_out_stay_readable_in_80_columns() {
        let mut state = demo();
        state.focus = Focus::Right;
        let stream = screen(&state, 80, 24).join("\n");
        assert!(stream.contains("Ctrl+] sidebar"), "{stream}");

        state.view = View::Scrollback;
        let scrollback = screen(&state, 80, 24).join("\n");
        assert!(scrollback.contains("PageUp/PageDown"), "{scrollback}");
        assert!(scrollback.contains("Esc back to live"), "{scrollback}");
        assert!(scrollback.contains("Ctrl+] sidebar"), "{scrollback}");
    }

    #[test]
    fn the_footer_hints_match_the_focused_pane() {
        let mut state = demo();
        let sidebar_hints = screen(&state, 100, 16).join("\n");
        assert!(sidebar_hints.contains("Ctrl+Q quit"), "{sidebar_hints}");
        assert!(sidebar_hints.contains("dbl-click open"), "{sidebar_hints}");
        state.focus = Focus::Right;
        let stream_hints = screen(&state, 100, 16).join("\n");
        assert!(stream_hints.contains("Ctrl+] sidebar"), "{stream_hints}");
        assert!(!stream_hints.contains("Shift+PageUp"), "{stream_hints}");
        assert!(!stream_hints.contains("Ctrl+Q quit"), "{stream_hints}");
        state.begin_settings();
        let settings_hints = screen(&state, 100, 16).join("\n");
        assert!(settings_hints.contains("Esc cancel"), "{settings_hints}");
    }

    #[test]
    fn a_notice_leads_the_footer_and_stays_out_of_the_stream() {
        let mut state = demo();
        state.focus = Focus::Right;
        state.notice = Some(crate::state::Notice::error("write failed"));
        let area = Rect::new(0, 0, 100, 16);
        let footer_top = (area.y + area.height - chrome(area).footer.height) as usize;
        let lines = screen(&state, 100, 16);
        let footer = lines[footer_top..].join("\n");
        assert!(footer.contains("write failed"), "{footer}");
        assert!(!lines[..footer_top]
            .iter()
            .any(|line| line.contains("write failed")));
    }

    #[test]
    fn the_baud_prompt_is_drawn_with_the_default_and_typed_digits() {
        let mut state = demo();
        state.select_path("/dev/ttyUSB0");
        assert!(state.begin_baud_prompt());
        state.prompt_push('9');
        state.prompt_push('6');
        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("Baud for /dev/ttyUSB0"), "{text}");
        assert!(text.contains("115200"), "{text}");
        assert!(text.contains("96"), "{text}");
    }

    #[test]
    fn the_sidebar_scrolls_to_keep_the_highlight_visible() {
        let mut state = AppState::new(12, 40);
        state.set_devices(
            (0..12)
                .map(|index| DeviceEntry::new(format!("/dev/ttyUSB{index}")))
                .collect(),
        );
        assert!(state.select_path("/dev/ttyUSB11"));
        let text = screen(&state, 80, 16).join("\n");
        assert!(text.contains("/dev/ttyUSB11"), "{text}");
        // The Settings button is still there at the bottom.
        assert!(text.contains("[ Settings ]"), "{text}");
    }

    #[test]
    fn a_short_terminal_keeps_the_stream_and_one_footer_row() {
        let mut state = demo();
        state.focus = Focus::Right;
        state.notice = Some(crate::state::Notice::error("boom"));
        let lines = screen(&state, 100, 5);
        assert!(lines[0].contains("SerialNexus"), "{lines:?}");
        assert!(lines[4].contains("boom"), "{lines:?}");

        let tiny = screen(&state, 20, 4);
        assert!(tiny[0].contains("SerialNexus"), "{tiny:?}");
    }

    #[test]
    fn the_sidebar_says_so_when_there_are_no_devices() {
        let state = AppState::new(12, 40);
        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("no serial devices"), "{text}");
        assert!(text.contains("no device shown"), "{text}");
        assert!(text.contains("[ Settings ]"), "{text}");
    }

    #[test]
    fn screen_cells_keep_the_device_style() {
        let mut state = demo();
        state.focus = Focus::Right;
        state.apply_event(&Event::device_output(7, b"\x1b[31mR\x1b[0mN"));
        let area = Rect::new(0, 0, 100, 16);
        let right = panes(area).right;
        let mut buffer = Buffer::empty(area);
        render(&state, &mut buffer, area);
        assert_eq!(buffer[(right.x, right.y)].symbol(), "R");
        assert_eq!(buffer[(right.x, right.y)].fg, RColor::Indexed(1));
    }

    #[test]
    fn wide_characters_are_measured_in_columns() {
        let mut state = demo();
        state.set_devices(vec![DeviceEntry::new(
            "/dev/界界界界界界界界界界界界界界界界界界界界",
        )]);
        let header = &screen(&state, 60, 10)[0];
        assert!(
            !header.contains("focus: sidebar"),
            "the focus hint gives way: {header:?}"
        );

        let mut state = demo();
        state
            .sessions
            .get_mut(&SessionKey::local(7))
            .unwrap()
            .control = Some("界界界界界界".into());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 16));
        render(&state, &mut buffer, Rect::new(0, 0, 120, 16));
        let header = &screen(&state, 120, 16)[0];
        assert_eq!(header.matches('界').count(), 6, "{header:?}");
    }

    fn active_share(state: &mut AppState) {
        use serial_nexus_protocol::{PairingRole, PairingSecret, ShareSessionInfo};
        let bind: std::net::SocketAddr = "127.0.0.1:7331".parse().unwrap();
        state.sharing.active = true;
        state.sharing.pairing = Some(ShareSessionInfo {
            session_id: "test".into(),
            bind,
            host: serial_nexus_protocol::PairedHost {
                address: bind,
                host_name: "127.0.0.1".into(),
                ca_cert_der_b64: "Y2E=".into(),
                host_cert_fingerprint_sha256: "00".repeat(32),
            },
            human: PairingSecret {
                role: PairingRole::Human,
                credential: "human-test".into(),
            },
            ai: PairingSecret {
                role: PairingRole::Ai,
                credential: "ai-test".into(),
            },
        });
    }

    #[test]
    fn the_share_scroll_limit_matches_the_keys_the_page_draws() {
        let mut state = demo();
        active_share(&mut state);
        state.resize(27, 103);
        let right = Rect::new(0, 0, 103, 27);
        let layout = share_layout(&state, right).expect("layout");
        let lines = pairing_lines(&state)
            .iter()
            .flat_map(|line| wrap_line(line, layout.details.width.max(1) as usize))
            .count();
        assert_eq!(
            share_max_scroll(&state),
            lines.saturating_sub(layout.details.height as usize),
            "the scroll clamp uses the same pane the renderer does"
        );
    }

    #[test]
    fn the_share_wheel_targets_the_form_or_the_key_details() {
        let mut state = demo();
        active_share(&mut state);
        state.begin_share();
        let area = Rect::new(0, 0, 80, 24);
        let layout = share_layout(&state, panes(area).right).expect("layout");
        assert_eq!(
            share_wheel_target(&state, area, layout.form.x + 1, layout.form.y),
            Some(ShareWheel::Form)
        );
        assert_eq!(
            share_wheel_target(
                &state,
                area,
                layout.details.x + 1,
                layout.details.y + layout.details.height.saturating_sub(1)
            ),
            Some(ShareWheel::Details)
        );
    }

    #[test]
    fn a_small_or_crowded_share_page_still_reserves_room_for_its_keys() {
        let mut state = demo();
        active_share(&mut state);
        let small = Rect::new(0, 0, 40, 12);
        let layout = share_layout(&state, panes(small).right).expect("layout");
        assert!(
            layout.details.height >= 3,
            "a 40x12 page keeps keys/details reachable: {layout:?}"
        );

        let mut crowded = demo();
        active_share(&mut crowded);
        crowded.set_devices(
            (0..13)
                .map(|index| DeviceEntry::new(format!("/dev/ttyUSB{index}")))
                .collect(),
        );
        let area = Rect::new(0, 0, 80, 24);
        let layout = share_layout(&crowded, panes(area).right).expect("layout");
        assert!(
            layout.details.height >= 3,
            "thirteen devices cannot consume the whole key area: {layout:?}"
        );
    }

    #[test]
    fn the_share_page_shows_fields_devices_and_an_explicit_start_button() {
        let mut state = demo();
        state.begin_share();
        let text = screen(&state, 100, 30).join("\n");
        assert!(text.contains("Share · off"), "{text}");
        assert!(text.contains("Host address"), "{text}");
        assert!(text.contains("Port"), "{text}");
        assert!(text.contains("[ ] /dev/ttyUSB1"), "{text}");
        assert!(text.contains("[ Start sharing ]"), "{text}");
        assert!(
            !text.contains("Human terminal key"),
            "nothing is shared before Start: {text}"
        );
        assert!(text.contains("Enter edit or activate"), "{text}");
    }

    #[test]
    fn an_active_endpoint_save_confirmation_names_disconnect_and_key_rotation() {
        let mut state = demo();
        state.begin_share();
        state.sharing.active = true;
        state.share_confirm = Some(crate::state::ShareConfirm::Rebind);
        state.draft.share_port = state.settings.share_port.saturating_add(1);
        let text = screen(&state, 100, 24).join("\\n");
        assert!(text.contains("[ Confirm change ]"), "{text}");
        assert!(text.contains("Remote clients disconnect"), "{text}");
        assert!(text.contains("fresh long keys are issued"), "{text}");
    }

    #[test]
    fn a_serious_warning_adds_a_visible_dismiss_row_and_a_key_hint() {
        let mut state = demo();
        state.warning("write may have reached /dev/ttyUSB1");
        let text = screen(&state, 100, 30).join("\n");
        assert!(text.contains("warning: write may have reached"), "{text}");
        assert!(text.contains("[ Dismiss warning ]"), "{text}");
        assert!(text.contains("x dismiss"), "{text}");
        let area = Rect::new(0, 0, 100, 30);
        let sidebar = panes(area).sidebar;
        let dismiss = sidebar_with_warning(sidebar, true)
            .expect("sidebar")
            .dismiss;
        assert_eq!(
            hit_test(&state, area, dismiss.x + 1, dismiss.y),
            Some(Hit::Dismiss)
        );
    }

    #[test]
    fn a_narrow_connect_page_keeps_the_older_fields_clickable() {
        let mut state = demo();
        state.connect = Some(ConnectForm::new());
        state.view = View::Connect;
        state.focus = Focus::Right;
        let area = Rect::new(0, 0, 40, 12);
        let text = screen(&state, 40, 12).join("\n");
        assert!(text.contains("[ Older fields ]"), "{text}");
        let row = text
            .lines()
            .position(|line| line.contains("[ Older fields ]"))
            .expect("the button row") as u16;
        let column = text
            .lines()
            .nth(row as usize)
            .and_then(|line| line.find("[ Older fields ]"))
            .expect("the button column") as u16;
        assert_eq!(
            connect_hit_test(&state, panes(area).right, column, row),
            Some(ConnectHit::ToggleMode)
        );
    }

    #[test]
    fn an_81_column_connect_page_does_not_clip_the_single_row_action_bar() {
        let mut state = demo();
        state.connect = Some(ConnectForm::new());
        state.view = View::Connect;
        state.focus = Focus::Right;
        let text = screen(&state, 80, 24).join("\n");
        assert!(text.contains("[ Older fields ]"), "{text}");
        assert!(
            !text
                .lines()
                .any(|line| line.contains("[ Older ") && !line.contains("[ Older fields ]")),
            "the label is not clipped:\n{text}"
        );
    }

    #[test]
    fn the_narrowest_connect_page_still_draws_every_action_target() {
        let mut state = demo();
        state.connect = Some(ConnectForm::new());
        state.view = View::Connect;
        state.focus = Focus::Right;
        let area = Rect::new(0, 0, 27, 12);
        let right = panes(area).right;
        let text = screen(&state, 27, 12).join("\n");
        for label in [
            "[ Connect ]",
            "[ Clear key ]",
            "[ Cancel ]",
            "[ Older fields ]",
        ] {
            assert!(text.contains(label), "{label} is drawn:\n{text}");
        }
        let mut found = Vec::new();
        for row in right.y..right.bottom() {
            for column in right.x..right.right() {
                if let Some(hit) = connect_hit_test(&state, right, column, row) {
                    if !found.contains(&hit) {
                        found.push(hit);
                    }
                }
            }
        }
        assert!(found.contains(&ConnectHit::Connect), "{found:?}");
        assert!(found.contains(&ConnectHit::ClearKey), "{found:?}");
        assert!(found.contains(&ConnectHit::Cancel), "{found:?}");
        assert!(found.contains(&ConnectHit::ToggleMode), "{found:?}");
    }

    #[test]
    fn a_short_connect_page_scrolls_to_the_selected_advanced_field() {
        let mut state = demo();
        state.connect = Some(ConnectForm::advanced());
        state.view = View::Connect;
        state.focus = Focus::Right;
        if let Some(form) = &mut state.connect {
            form.field = ConnectField::Credential;
            form.credential = "pinned-secret".into();
        }
        let text = screen(&state, 80, 10).join("\n");
        assert!(
            text.contains("Human pairing credential"),
            "the selected field is drawn, not hidden above the viewport:\n{text}"
        );
    }

    #[test]
    fn a_short_share_page_still_shows_an_action_failure() {
        let mut state = demo();
        state.begin_share();
        state.share_error = Some("check at least one device before starting".into());
        let text = screen(&state, 80, 8).join("\n");
        assert!(
            text.contains("check at least one device"),
            "a compact page shows the action failure:\n{text}"
        );
    }

    #[test]
    fn a_very_short_sidebar_still_exposes_a_mouse_dismiss_target() {
        let mut state = demo();
        state.warning("write uncertain");
        let area = Rect::new(0, 0, 40, 7);
        let sidebar = panes(area).sidebar;
        let dismiss = sidebar_with_warning(sidebar, true)
            .expect("sidebar")
            .dismiss;
        assert!(dismiss.height > 0, "the Dismiss row is drawn: {sidebar:?}");
        assert_eq!(
            hit_test(&state, area, dismiss.x + 1, dismiss.y),
            Some(Hit::Dismiss)
        );
    }

    #[test]
    fn the_connect_page_shows_visible_connect_and_clear_key_buttons() {
        let mut state = demo();
        state.connect = Some(ConnectForm::new());
        state.view = View::Connect;
        let text = screen(&state, 100, 30).join("\n");
        assert!(text.contains("[ Connect ]"), "{text}");
        assert!(text.contains("[ Clear key ]"), "{text}");
        assert!(text.contains("[ Cancel ]"), "{text}");
        assert!(text.contains("Ctrl+L"), "{text}");
    }
}
