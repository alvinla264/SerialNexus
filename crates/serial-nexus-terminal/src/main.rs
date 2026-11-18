//! `serial-nexus-terminal`: a minicom-like human TUI for SerialNexus.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind, MouseEvent};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use serial_nexus_client::{Client, ClientConfig, ClientInfo, GatewayConfig, GatewayServer};
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::input::translate;
use serial_nexus_terminal::render::{render, stream_body_area};
use serial_nexus_terminal::settings::{SettingsStore, TerminalSettings};
use serial_nexus_terminal::state::{ClipboardCopy, Notice};
use serial_nexus_terminal::terminal::{CleanupGuard, CrosstermOps};

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("serial-nexus-terminal: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Own the per-user MCP gateway for exactly this foreground terminal
    // lifetime. A second supported terminal fails before touching the core.
    let gateway = GatewayServer::start(GatewayConfig::default())?;

    // The client library's connect seam starts serial-nexus-core when needed.
    let config = ClientConfig::new(ClientInfo::human("serial-nexus-terminal"));
    let client = Client::connect(config)?;

    // Settings are a convenience: an unreadable or invalid file leaves the
    // defaults in effect, is reported, and is never overwritten.
    let (settings, store, settings_error) = match SettingsStore::from_env() {
        Some(store) => match store.load_or_create() {
            Ok(settings) => (settings, Some(store), None),
            Err(error) => (
                TerminalSettings::default(),
                // Changes are not saved over a file the human hand-edited into
                // an invalid state: they apply to this launch only.
                None,
                Some(format!("settings {}: {error}", store.path().display())),
            ),
        },
        None => (TerminalSettings::default(), None, None),
    };
    let mut app = App::with_preferences(client, 24, 80, settings, store);
    if let Some(message) = settings_error {
        app.state.set_notice(Notice::error(message));
    }

    // The guard restores raw mode and the alternate screen on every exit path,
    // including event-loop errors and panics.
    let guard = CleanupGuard::new(CrosstermOps)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = event_loop(&mut terminal, &mut app);

    app.shutdown();
    drop(gateway);
    drop(terminal);
    // Drop the guard before printing an error, so it lands on a sane terminal.
    drop(guard);
    result.map_err(Into::into)
}

/// Ask the terminal to copy one complete key with OSC 52. The key never
/// appears in the drawn screen, a file or a log.
fn copy_key_to_clipboard(copy: &ClipboardCopy) -> io::Result<()> {
    use base64::Engine as _;
    use std::io::Write;
    let payload = base64::engine::general_purpose::STANDARD.encode(&copy.key);
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{payload}\x1b\\")?;
    stdout.flush()
}

/// Show one complete key as a single soft-wrapped line.
///
/// The key is not saved and stays only on this alternate screen; any key
/// returns to the normal view.
fn show_key_for_copy(view: &ClipboardCopy) -> io::Result<()> {
    let mut stdout = io::stdout();
    // Mouse capture would turn a drag into motion events (and the terminal
    // could not select text), so the terminal owns the mouse while the key is
    // shown. Only a key press returns: motion and resize must not dismiss the
    // view before a selection is copied.
    crossterm::execute!(stdout, crossterm::event::DisableMouseCapture)?;
    let result = (|| {
        let size = crossterm::terminal::size()?;
        draw_key_for_copy(&mut stdout, view, size)?;
        loop {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => break,
                Event::Resize(columns, rows) => {
                    draw_key_for_copy(&mut stdout, view, (columns, rows))?
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let restored = crossterm::execute!(stdout, crossterm::event::EnableMouseCapture);
    result.and(restored)
}

fn draw_key_for_copy(
    stdout: &mut io::Stdout,
    view: &ClipboardCopy,
    (columns, rows): (u16, u16),
) -> io::Result<()> {
    use std::io::Write;
    write!(stdout, "\x1b[2J\x1b[H")?;
    let key_rows = view.key.len().div_ceil(columns.max(1) as usize) + 4;
    if key_rows > rows as usize {
        // Never draw a key that does not fit: rows would scroll off and the
        // selection could not contain the whole string. After enlarging, the
        // person presses v again and the whole line is shown.
        writeln!(
            stdout,
            "SerialNexus {} — this pane is too small for the whole key. Press any key, enlarge the pane, then activate its Show key button again.",
            view.label
        )?;
        stdout.flush()?;
        return Ok(());
    }
    writeln!(
        stdout,
        "SerialNexus {} — select the whole line below and copy it, then press any key to return.",
        view.label
    )?;
    writeln!(stdout)?;
    // No newline until after the key: the terminal wraps it as one logical
    // line, which terminals that mark wraps copy without inserting line
    // breaks.
    write!(stdout, "{}", view.key)?;
    writeln!(stdout)?;
    writeln!(stdout)?;
    stdout.flush()
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    let mut last_refresh = Instant::now();
    let mut last_area = Rect::default();
    loop {
        app.pump();
        // Ordinary notices clear themselves; serious warnings wait for the
        // sidebar's explicit Dismiss action.
        app.state.expire_notices(Instant::now());
        while let Some(copy) = app.take_clipboard_copy() {
            // OSC 52 asks the terminal to set its clipboard; a terminal that
            // does not support it simply ignores the sequence.
            let _ = copy_key_to_clipboard(&copy);
        }
        if let Some(view) = app.state.show_key.take() {
            // A terminal-independent fallback: one logical line that the
            // terminal soft-wraps, so selecting it copies the key without
            // inserted newlines. Any input returns to the TUI.
            show_key_for_copy(&view)?;
            terminal.clear()?;
            continue;
        }
        terminal.draw(|frame| {
            let area = frame.area();
            last_area = area;
            if let Some(body) = stream_body_area(area) {
                app.resize_stream(body.height as usize, body.width as usize);
            }
            if let Some((x, y)) = render(&app.state, frame.buffer_mut(), area) {
                frame.set_cursor_position((x, y));
            }
        })?;
        if app.state.quit {
            return Ok(());
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                event::Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(key) = translate(key) {
                        app.handle_key(key);
                    }
                }
                event::Event::Paste(text) => app.handle_paste(&text),
                event::Event::Mouse(mouse) => handle_mouse(app, last_area, mouse),
                event::Event::Resize(_, _) => {
                    app.handle_resize();
                    terminal.autoresize()?;
                }
                _ => {}
            }
        }
        if last_refresh.elapsed() >= Duration::from_secs(2) {
            app.refresh_devices();
            last_refresh = Instant::now();
        }
    }
}

/// A click on a device row highlights it; a double-click opens or joins it,
/// just as Enter does. A click on the sidebar's Settings button opens the
/// settings. In the scrollback view, the wheel scrolls the kept output.
fn handle_mouse(app: &mut App, area: Rect, mouse: MouseEvent) {
    app.handle_mouse(area, mouse.kind, mouse.column, mouse.row, Instant::now());
}
