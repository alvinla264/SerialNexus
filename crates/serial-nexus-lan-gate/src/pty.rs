//! A real process on a pseudo-terminal, with its drawn screen.
//!
//! Used for the real `serial-nexus-terminal` binary on both containers: the
//! host's terminal is driven like a human's keyboard with the side effects
//! read from the screen, and the client's terminal is only kept alive because
//! it hosts the MCP gateway. `vt100` turns what the child wrote back into the
//! screen a human would see.
//!
//! Diagnostics never include the screen: the host's pairing view is on it.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::pty::{openpty, Winsize};
use vt100::Parser;

pub const KEY_ESC: &[u8] = b"\x1b";
pub const KEY_CTRL_Q: &[u8] = &[0x11];
pub const KEY_CTRL_S: &[u8] = &[0x13];
/// What a terminal without the enhanced keyboard protocol sends for `Ctrl+]`.
pub const KEY_CTRL_BRACKET: &[u8] = &[0x1d];

/// Every screen on this harness may carry pairing material, so no error ever
/// includes it.
const SCREEN_OMITTED: &str = "[screen omitted: it may contain pairing credentials]";

pub struct Tui {
    master: File,
    child: Child,
    parser: Arc<Mutex<Parser>>,
    raw: Arc<Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    rows: u16,
    cols: u16,
}

impl Tui {
    /// Start `binary` with `environment` exactly as given (plus `TERM`), on a
    /// fresh PTY whose slave is its controlling terminal and stdio.
    pub fn spawn(
        binary: &Path,
        environment: &[(String, String)],
        removed: &[&str],
        rows: u16,
        cols: u16,
    ) -> Result<Self, String> {
        let pty = openpty(
            Some(&Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .map_err(|error| format!("cannot open a pty: {error}"))?;
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        for file in [&master, &slave] {
            let flags = fcntl(file.as_raw_fd(), FcntlArg::F_GETFD)
                .map_err(|error| format!("cannot read fd flags: {error}"))?;
            fcntl(
                file.as_raw_fd(),
                FcntlArg::F_SETFD(FdFlag::from_bits_truncate(flags) | FdFlag::FD_CLOEXEC),
            )
            .map_err(|error| format!("cannot set close-on-exec: {error}"))?;
        }

        let mut command = Command::new(binary);
        for name in removed {
            command.env_remove(name);
        }
        for (name, value) in environment {
            command.env(name, value);
        }
        command
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().map_err(io_error)?))
            .stdout(Stdio::from(slave.try_clone().map_err(io_error)?))
            .stderr(Stdio::from(slave.try_clone().map_err(io_error)?));
        // A new session with the slave as its controlling terminal, so raw
        // mode, mouse capture and SIGWINCH all take their real path.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid().map_err(io::Error::from)?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
        drop(slave);

        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, 0)));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let mut reads = master.try_clone().map_err(io_error)?;
        let screen = Arc::clone(&parser);
        let transcript = Arc::clone(&raw);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reads.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        let mut kept = transcript.lock().expect("transcript lock");
                        // Bound the transcript: long runs must not grow without end.
                        if kept.len() < 4 * 1024 * 1024 {
                            kept.extend_from_slice(&buffer[..read]);
                        }
                        drop(kept);
                        screen.lock().expect("screen lock").process(&buffer[..read]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            master,
            child,
            parser,
            raw,
            reader: Some(reader),
            rows,
            cols,
        })
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.master
            .write_all(bytes)
            .and_then(|()| self.master.flush())
            .map_err(|error| format!("cannot type into the terminal: {error}"))
    }

    /// A left click at a zero-based screen cell, as SGR mouse reporting.
    pub fn click(&mut self, column: u16, row: u16) -> Result<(), String> {
        self.send(
            format!(
                "\x1b[<0;{};{}M\x1b[<0;{};{}m",
                column + 1,
                row + 1,
                column + 1,
                row + 1
            )
            .as_bytes(),
        )
    }

    pub fn text(&self) -> String {
        self.parser.lock().expect("screen lock").screen().contents()
    }

    pub fn row(&self, row: u16) -> String {
        self.text()
            .lines()
            .nth(row as usize)
            .unwrap_or_default()
            .trim_end()
            .to_string()
    }

    /// The cell a character sits at, as `(column, row)`.
    pub fn find(&self, needle: &str) -> Option<(u16, u16)> {
        use unicode_width::UnicodeWidthStr;

        let parser = self.parser.lock().expect("screen lock");
        for (row, line) in parser.screen().rows(0, self.cols).enumerate() {
            if let Some(byte) = line.find(needle) {
                let column = line[..byte].width() as u16;
                return Some((column, row as u16));
            }
        }
        None
    }

    pub fn wait_for_text(&mut self, needle: &str, timeout: Duration) -> Result<(), String> {
        self.wait_until(&format!("the screen to show {needle:?}"), timeout, |text| {
            text.contains(needle)
        })
    }

    pub fn wait_until(
        &mut self,
        what: &str,
        timeout: Duration,
        mut ready: impl FnMut(&str) -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if ready(&self.text()) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out waiting for {what}; {SCREEN_OMITTED}"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Everything the child has written to its terminal, before the emulator
    /// turns it into a screen. Used for OSC 52 copy requests.
    pub fn raw_transcript(&self) -> Vec<u8> {
        self.raw
            .lock()
            .map(|bytes| bytes.clone())
            .unwrap_or_default()
    }

    /// Whether the child is still on the TUI's alternate screen.
    pub fn in_alternate_screen(&self) -> bool {
        self.parser
            .lock()
            .expect("screen lock")
            .screen()
            .alternate_screen()
    }

    /// Wait for the child to exit without sending anything.
    pub fn wait_exit(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(error) => return Err(format!("cannot wait for the terminal: {error}")),
            }
            if Instant::now() >= deadline {
                return Err("the terminal did not exit in time".to_string());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn io_error(error: io::Error) -> String {
    format!("pty setup failed: {error}")
}
