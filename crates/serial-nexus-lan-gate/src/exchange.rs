//! The two containers' private exchange directory.
//!
//! The host writes one small announcement per sharing session (bound endpoint
//! and the two single-use pairing keys) and the client writes one numbered
//! command at a time; the host acknowledges each command. Every file is a
//! regular file owned by the writing user and readable by its owner only. The
//! directory is a bind mount from the orchestrator's private temporary
//! directory, so nothing here touches a real user configuration.
//!
//! The orchestrator never reads these files: diagnostics come from the gate's
//! own redacted log lines, so a key cannot end up in a container log or a CI
//! transcript.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Most a state file may be. Every file here is a few hundred bytes.
pub const MAX_STATE_BYTES: u64 = 16 * 1024;

/// One sharing session's pairing material, written by the host container.
///
/// The keys are session-only and useless once the host stops sharing; the
/// host removes the file on shutdown.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Announcement {
    /// The container-private IPv4 address the listener is bound to.
    pub address: String,
    /// The actual bound port (`0` in settings asks the kernel).
    pub port: u16,
    /// The human terminal sharing key (`snx1.human.…`).
    pub human_key: String,
    /// The MCP/AI sharing key (`snx1.ai.…`).
    pub ai_key: String,
}

/// The announcement for sharing session `generation` (1-based).
pub fn announcement_path(state: &Path, generation: u32) -> PathBuf {
    state.join(format!("announcement-{generation}.json"))
}

/// The single command slot the client writes.
pub fn command_path(state: &Path) -> PathBuf {
    state.join("host.cmd")
}

/// The host's answer to the command with the same sequence number.
pub fn acknowledgement_path(state: &Path) -> PathBuf {
    state.join("host.ack")
}

/// Written by the client once every number is verified. The host keeps its
/// container, and therefore its core, alive until this arrives, so the
/// observed disconnection cannot be caused by the container simply stopping.
pub fn completion_path(state: &Path) -> PathBuf {
    state.join("client.done")
}

/// What the completion file says when the client passed every step.
pub const COMPLETION_OK: &[u8] = b"all steps passed\n";

/// Create `path` as a private directory (0700), or verify the existing one is.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.is_dir() {
                return Err(io::Error::other(format!(
                    "{} exists and is not a directory",
                    path.display()
                )));
            }
            if metadata.uid() != effective_uid() {
                return Err(io::Error::other(format!(
                    "{} is owned by uid {}, not {}",
                    path.display(),
                    metadata.uid(),
                    effective_uid()
                )));
            }
            if metadata.mode() & 0o077 != 0 {
                return Err(io::Error::other(format!(
                    "{} is accessible by group or other; it must be 0700",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Write `bytes` to `path` atomically, readable by its owner only.
///
/// A sibling staging file is created exclusively with mode 0600, synced and
/// renamed over the destination, then the directory is synced. A reader never
/// sees a partial file.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("the state file has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the state file has no name"))?
        .to_string_lossy()
        .into_owned();
    let staging = directory.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&staging, path)?;
        File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

/// Read a state file, refusing anything that is not a private regular file.
pub fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if metadata.uid() != effective_uid() {
        return Err(io::Error::other(format!(
            "{} is owned by uid {}, not {}",
            path.display(),
            metadata.uid(),
            effective_uid()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is readable by group or other; it must be 0600",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::other(format!(
            "{} is larger than {MAX_STATE_BYTES} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Read a state file, treating an absent file as "not yet".
pub fn read_optional_private(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match read_private(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Remove a state file if it exists. Keys are not kept after the session.
pub fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// `"<sequence> <command>"` as the client writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub sequence: u64,
    pub name: String,
}

impl Command {
    pub fn encode(&self) -> String {
        format!("{} {}\n", self.sequence, self.name)
    }

    /// Parse one command line; a malformed line is an error, not a guess.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text =
            std::str::from_utf8(bytes).map_err(|_| "the command is not UTF-8".to_string())?;
        let line = text.lines().next().unwrap_or_default().trim();
        let (sequence, name) = line
            .split_once(' ')
            .ok_or_else(|| "the command is not '<sequence> <name>'".to_string())?;
        let sequence = sequence
            .parse::<u64>()
            .map_err(|_| "the command sequence is not a number".to_string())?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
        {
            return Err("the command name is not lowercase letters".to_string());
        }
        Ok(Self {
            sequence,
            name: name.to_string(),
        })
    }
}

/// `"<sequence> ok"` or `"<sequence> failed: <reason>"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acknowledgement {
    pub sequence: u64,
    pub failure: Option<String>,
}

impl Acknowledgement {
    pub fn ok(sequence: u64) -> String {
        format!("{sequence} ok\n")
    }

    pub fn failed(sequence: u64, reason: &str) -> String {
        // Keep one line, and never let a reason smuggle a newline into the file.
        let reason = reason.replace(['\n', '\r'], " ");
        format!("{sequence} failed: {reason}\n")
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "the ack is not UTF-8".to_string())?;
        let line = text.lines().next().unwrap_or_default().trim_end();
        let (sequence, rest) = line
            .split_once(' ')
            .ok_or_else(|| "the ack is not '<sequence> <result>'".to_string())?;
        let sequence = sequence
            .parse::<u64>()
            .map_err(|_| "the ack sequence is not a number".to_string())?;
        let failure = if rest == "ok" {
            None
        } else if let Some(reason) = rest.strip_prefix("failed: ") {
            Some(reason.to_string())
        } else {
            return Err("the ack result is not ok or failed".to_string());
        };
        Ok(Self { sequence, failure })
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { nix::libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_dir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("state");
        ensure_private_dir(&path).expect("private dir");
        directory
    }

    #[test]
    fn announcements_round_trip_through_a_private_file() {
        let directory = private_dir();
        let state = directory.path().join("state");
        let path = announcement_path(&state, 1);
        let announcement = Announcement {
            address: "172.18.0.2".to_string(),
            port: 7331,
            human_key: "snx1.human.abc".to_string(),
            ai_key: "snx1.ai.def".to_string(),
        };
        write_private(&path, serde_json::to_vec(&announcement).unwrap().as_slice()).expect("write");
        let metadata = fs::metadata(&path).expect("metadata");
        assert_eq!(metadata.mode() & 0o777, 0o600);
        let loaded: Announcement = serde_json::from_slice(&read_private(&path).unwrap()).unwrap();
        assert_eq!(loaded.port, 7331);
        assert!(read_optional_private(&announcement_path(&state, 2))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_world_readable_state_file_is_refused() {
        let directory = private_dir();
        let state = directory.path().join("state");
        let path = announcement_path(&state, 1);
        write_private(&path, b"{}").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let error = read_private(&path).expect_err("must refuse");
        assert!(error.to_string().contains("0600"), "{error}");
    }

    #[test]
    fn a_symlinked_state_file_is_refused() {
        let directory = private_dir();
        let state = directory.path().join("state");
        let target = state.join("target");
        write_private(&target, b"{}").expect("write");
        let link = announcement_path(&state, 1);
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(read_private(&link).is_err());
    }

    #[test]
    fn commands_and_acks_are_strictly_parsed() {
        let command = Command::parse(b"3 uncheck\n").expect("parse");
        assert_eq!(
            command,
            Command {
                sequence: 3,
                name: "uncheck".into()
            }
        );
        assert!(Command::parse(b"3 Uncheck").is_err());
        assert!(Command::parse(b"x uncheck").is_err());
        assert!(Command::parse(b"3").is_err());

        let ack = Acknowledgement::parse(&write_ack_bytes(3, Some("bad thing"))).expect("ack");
        assert_eq!(ack.sequence, 3);
        assert_eq!(ack.failure.as_deref(), Some("bad thing"));
        assert!(Acknowledgement::parse(b"3 maybe").is_err());
    }

    fn write_ack_bytes(sequence: u64, failure: Option<&str>) -> Vec<u8> {
        match failure {
            Some(reason) => Acknowledgement::failed(sequence, reason).into_bytes(),
            None => Acknowledgement::ok(sequence).into_bytes(),
        }
    }

    #[test]
    fn a_public_directory_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("state");
        fs::create_dir(&path).expect("dir");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(ensure_private_dir(&path).is_err());
    }
}
