//! A container-local serial echo device, with no hardware and no host device.
//!
//! The simulator owns a PTY pair and publishes the slave as a
//! `/dev/serial/by-id/...` symlink. That is exactly the path production
//! discovery reads, so the real core enumerates it as an ordinary serial
//! device; nothing in production is overridden or weakened. The slave is a
//! pseudo-terminal, not a USB adapter, and the sim writes anything the core
//! writes back to it, so an exact echo can be asserted end to end.
//!
//! The symlink lives only in the container's own `/dev`; it is created after
//! the container starts and disappears with it.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::symlink;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::openpty;

/// What the simulator prints once, so a remote history read has deterministic
/// device output even before anyone writes.
pub const READY_BANNER: &str = "SNX-ECHO READY\r\n";

pub struct EchoSim {
    /// The canonical slave path the core opens (`/dev/pts/N`).
    path: String,
    _master: File,
    _slave: File,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EchoSim {
    /// Create the PTY pair and publish it under `by_id_dir`/`name`.
    ///
    /// `by_id_dir` is production's `/dev/serial/by-id` in the container; the
    /// caller owns creating it.
    pub fn start(by_id_dir: &Path, name: &str) -> io::Result<Self> {
        let pty = openpty(None, None)?;
        let master: File = pty.master.into();
        let slave: File = pty.slave.into();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))?;
        let path = path
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::other("the pty path is not UTF-8"))?;

        // A real serial port comes up raw: the core configures the opened
        // device that way. Doing it here too keeps the pty's line discipline
        // (echo, CR/LF translation) from turning the simulator into an echo
        // loop before the core has opened it.
        let mut terminal = nix::sys::termios::tcgetattr(&slave)
            .map_err(|error| io::Error::other(error.to_string()))?;
        nix::sys::termios::cfmakeraw(&mut terminal);
        nix::sys::termios::tcsetattr(&slave, nix::sys::termios::SetArg::TCSANOW, &terminal)
            .map_err(|error| io::Error::other(error.to_string()))?;

        fcntl(master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        // A mishap in this process must not leak the device's ends to children
        // other than the core that opens the path itself.
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            let flags = fcntl(fd, FcntlArg::F_GETFD)?;
            fcntl(
                fd,
                FcntlArg::F_SETFD(
                    nix::fcntl::FdFlag::from_bits_truncate(flags) | nix::fcntl::FdFlag::FD_CLOEXEC,
                ),
            )?;
        }

        std::fs::create_dir_all(by_id_dir)?;
        let link = by_id_dir.join(name);
        match std::fs::remove_file(&link) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        symlink(&path, &link)?;

        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::clone(&stop);
        let mut reads = master.try_clone()?;
        let mut writes = master.try_clone()?;
        let thread = std::thread::spawn(move || {
            // The banner waits in the pty until the core opens the device.
            write_all_blocking(&mut writes, READY_BANNER.as_bytes());
            let mut buffer = [0u8; 1024];
            while !running.load(Ordering::Relaxed) {
                match reads.read(&mut buffer) {
                    Ok(0) => std::thread::sleep(Duration::from_millis(5)),
                    Ok(read) => write_all_blocking(&mut writes, &buffer[..read]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            path,
            _master: master,
            _slave: slave,
            stop,
            thread: Some(thread),
        })
    }

    /// The canonical path the core reports for the device.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for EchoSim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn write_all_blocking(file: &mut File, mut bytes: &[u8]) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !bytes.is_empty() {
        match file.write(bytes) {
            Ok(0) => return,
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Read;

    /// Open the simulated device the way the core does: by its published path.
    fn open_device(path: &str) -> File {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open the simulated device");
        fcntl(file.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).expect("nonblocking");
        file
    }

    #[test]
    fn exact_bytes_are_echoed_and_the_device_is_published_by_id() {
        let directory = tempfile::tempdir().expect("tempdir");
        let by_id = directory.path().join("by-id");
        let sim = EchoSim::start(&by_id, "snx-test-echo").expect("simulator");
        let link = by_id.join("snx-test-echo");
        assert_eq!(
            std::fs::read_link(&link).expect("link"),
            std::path::PathBuf::from(sim.path())
        );

        let mut device = open_device(sim.path());
        let banner = read_until(&mut device, READY_BANNER.as_bytes());
        assert!(banner.ends_with(READY_BANNER.as_bytes()));

        // What the core writes to the device comes back unchanged.
        device.write_all(b"ping\r\n").expect("write");
        let echoed = read_until(&mut device, b"ping\r\n");
        assert!(echoed.ends_with(b"ping\r\n"), "{echoed:?}");
    }

    #[test]
    fn two_devices_get_distinct_links() {
        let directory = tempfile::tempdir().expect("tempdir");
        let by_id = directory.path().join("by-id");
        let first = EchoSim::start(&by_id, "snx-echo-a").expect("first");
        let second = EchoSim::start(&by_id, "snx-echo-b").expect("second");
        assert_ne!(first.path(), second.path());
        assert!(by_id.join("snx-echo-a").exists());
        assert!(by_id.join("snx-echo-b").exists());
    }

    fn read_until(file: &mut File, needle: &[u8]) -> Vec<u8> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        loop {
            let mut buffer = [0u8; 256];
            match file.read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    got.extend_from_slice(&buffer[..read]);
                    if got.windows(needle.len()).any(|window| window == needle) {
                        return got;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => panic!("read failed: {error}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "did not see {needle:?}; got {got:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
