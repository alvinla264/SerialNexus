//! The SerialNexus Docker LAN gate.
//!
//! One image, two roles. The host role runs the real `serial-nexus-terminal`
//! on its own core with a container-local PTY echo device published through
//! production's `/dev/serial/by-id` discovery, starts sharing through the
//! terminal's own pairing controls, and answers the client's step commands.
//! The client role runs the production `RemoteClient` adapter and the real
//! `serial-nexus-mcp` binary through the real client terminal's gateway.
//!
//! This binary is test infrastructure. It adds no production discovery
//! override, no ACL shortcut and no TLS relaxation; the only unusual thing it
//! does is create a `/dev/serial/by-id` symlink inside the container it was
//! started in. See `docs/lan-docker.md`.

mod client;
mod echo;
mod exchange;
mod host;
mod mcp;
mod pty;
mod redact;

use std::path::PathBuf;
use std::process::ExitCode;

/// The two containers exchange state here; the orchestrator bind-mounts a
/// private temporary directory over it.
pub const STATE_DIR: &str = "/state";

/// Where the image installs the real binaries.
pub const BIN_DIR: &str = "/opt/snx/bin";

/// Print one non-secret progress line. Every message goes through redaction.
pub fn note(message: &str) {
    println!("lan-gate: {}", redact::redact(message));
}

pub struct Binaries {
    pub terminal: PathBuf,
    pub mcp: PathBuf,
    pub core: PathBuf,
}

impl Binaries {
    fn from_env() -> Result<Self, String> {
        let directory = std::env::var_os("SNX_LAN_GATE_BIN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(BIN_DIR));
        let terminal = directory.join("serial-nexus-terminal");
        let mcp = directory.join("serial-nexus-mcp");
        let core = directory.join("serial-nexus-core");
        for binary in [&terminal, &mcp, &core] {
            if !binary.is_file() {
                return Err(format!("missing binary {}", binary.display()));
            }
        }
        Ok(Self {
            terminal,
            mcp,
            core,
        })
    }
}

fn state_dir() -> PathBuf {
    std::env::var_os("SNX_LAN_GATE_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(STATE_DIR))
}

fn main() -> ExitCode {
    let role = std::env::args().nth(1).unwrap_or_default();
    let result = match role.as_str() {
        "host" => Binaries::from_env().and_then(|binaries| host::run(&binaries, &state_dir())),
        "client" => Binaries::from_env().and_then(|binaries| client::run(&binaries, &state_dir())),
        other => Err(format!(
            "usage: serial-nexus-lan-gate <host|client> (got {other:?})"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            println!("lan-gate: FAIL: {}", redact::redact(&message));
            ExitCode::FAILURE
        }
    }
}
