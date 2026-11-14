//! Private files for transferring a sharing key when the terminal cannot put
//! it on the clipboard.
//!
//! A key file is a small owner-only file (`0600`) in a private directory on a
//! memory-backed filesystem (`XDG_RUNTIME_DIR`, or `/dev/shm`). The Connect and
//! MCP pairing forms accept `file:/absolute/path` in the key field and read the
//! key from there, so a person can move the file privately (USB, `scp`) instead
//! of selecting hard-wrapped screen text. The file is removed when sharing
//! stops or the terminal exits; it is never written to durable storage.

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serial_nexus_protocol::{PairingRole, MAX_SHARING_KEY_BYTES};

/// A private, memory-backed directory for the temporary key files.
///
/// `XDG_RUNTIME_DIR` is per-user and cleared at logout. Without it, use a
/// per-user directory on `/dev/shm` (also tmpfs); never a disk-backed config
/// directory.
pub fn key_directory() -> Option<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(runtime).join("serial-nexus/pairing-keys"));
    }
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        Some(
            shm.join(format!("serial-nexus-{}", unsafe { libc::geteuid() }))
                .join("pairing-keys"),
        )
    } else {
        None
    }
}

/// Remove the temporary key files when the session ends.
pub fn clear_key_files() {
    let Some(directory) = key_directory() else {
        return;
    };
    for role in [PairingRole::Human, PairingRole::Ai] {
        let Some(name) = key_name(role) else { continue };
        let _ = std::fs::remove_file(directory.join(name));
    }
    let _ = std::fs::remove_dir(directory);
}

/// The default file for one role's key, when the environment names a config
/// directory.
pub fn key_file_path(role: PairingRole) -> Option<PathBuf> {
    key_directory().map(|directory| directory.join(key_name(role).unwrap_or("key")))
}

fn key_name(role: PairingRole) -> Option<&'static str> {
    match role {
        PairingRole::Human => Some("human.key"),
        PairingRole::Ai => Some("ai.key"),
    }
}

/// The path in a key field that starts with `file:`, if any.
pub fn file_reference(text: &str) -> Option<&str> {
    text.trim().strip_prefix("file:").map(str::trim)
}

/// Resolve the key text typed into a key field. A `file:` reference is read
/// with the same private rules the host uses to write it; anything else is
/// returned unchanged for the strict key parser.
pub fn resolve_key_text(text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    let Some(reference) = file_reference(trimmed) else {
        return Ok(trimmed.to_string());
    };
    if !reference.starts_with('/') {
        return Err("a key file path must be absolute, for example file:/home/you/key".to_string());
    }
    read_key_file(Path::new(reference), MAX_SHARING_KEY_BYTES)
}

/// Read a key from an owner-only regular file, never following a symlink.
pub fn read_key_file(path: &Path, max_bytes: usize) -> Result<String, String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        // As for the MCP pairing file: refuse a symlink, a device or a FIFO,
        // and never block on something planted at the path.
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot read the key file {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot read the key file {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "the key file {} must be a regular file, not a link or device",
            path.display()
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "the key file {} must be owned by you",
            path.display()
        ));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "the key file {} has mode {mode:04o}; make it readable by you only (chmod 600)",
            path.display()
        ));
    }
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(max_bytes as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("cannot read the key file {}: {error}", path.display()))?;
    if text.len() > max_bytes {
        return Err(format!(
            "the key file {} is larger than {max_bytes} bytes",
            path.display()
        ));
    }
    Ok(text.trim().to_string())
}

/// Write one key to an owner-only file, atomically and durably.
pub fn write_key_file(path: &Path, key: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "the key file has no directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let mut permissions = std::fs::metadata(parent)
        .map_err(|error| format!("cannot inspect {}: {error}", parent.display()))?
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(parent, permissions)
        .map_err(|error| format!("cannot protect {}: {error}", parent.display()))?;

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "key".to_string());
    let moment = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    let staging = path.with_file_name(format!(".{name}.{}.{moment}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .map_err(|error| format!("cannot write {}: {error}", staging.display()))?;
    let written = file
        .write_all(key.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("cannot write {}: {error}", staging.display()));
    }
    if let Err(error) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("cannot write {}: {error}", path.display()));
    }
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        format!("snx1.human.{}", "A".repeat(300))
    }

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn a_key_file_round_trips_owner_only() {
        let directory = temp();
        let path = directory.path().join("pairing/human.key");
        write_key_file(&path, &key()).expect("write");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .expect("dir metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            read_key_file(&path, MAX_SHARING_KEY_BYTES).expect("read"),
            key()
        );
    }

    #[test]
    fn unsafe_modes_links_and_oversize_files_are_refused_without_contents() {
        let directory = temp();
        let path = directory.path().join("human.key");
        write_key_file(&path, &key()).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let error = read_key_file(&path, MAX_SHARING_KEY_BYTES).expect_err("unsafe mode");
        assert!(error.contains("600"), "{error}");
        assert!(!error.contains(&key()), "{error}");

        let real = directory.path().join("real.key");
        write_key_file(&real, &key()).expect("write real");
        let link = directory.path().join("link.key");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert!(read_key_file(&link, MAX_SHARING_KEY_BYTES).is_err());

        let big = directory.path().join("big.key");
        std::fs::write(&big, "A".repeat(MAX_SHARING_KEY_BYTES + 1)).expect("big");
        std::fs::set_permissions(&big, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let error = read_key_file(&big, MAX_SHARING_KEY_BYTES).expect_err("oversize");
        assert!(error.contains("larger"), "{error}");
    }

    #[test]
    fn a_file_reference_must_be_absolute_and_resolves_the_trimmed_text() {
        let directory = temp();
        let path = directory.path().join("human.key");
        write_key_file(&path, &key()).expect("write");
        let reference = format!("file:{}", path.display());
        assert_eq!(
            resolve_key_text(&format!(" {reference} ")).expect("resolve"),
            key()
        );
        assert!(resolve_key_text("file:relative.key").is_err());
        assert_eq!(resolve_key_text("not-a-file").expect("plain"), "not-a-file");
    }
}
