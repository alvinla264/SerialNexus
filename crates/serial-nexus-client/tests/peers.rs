//! The private per-endpoint pairing file: only the owner may use it, and an
//! endpoint matches exactly one pairing.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};

use serial_nexus_client::{PairingRole, PeerError, Peers, RemoteEndpoint, MAX_PEERS_FILE_BYTES};
use tempfile::TempDir;

const CREDENTIAL: &str = "c2VjcmV0LWNyZWRlbnRpYWwtZm9yLXRlc3Rz";

fn peer_json(address: &str, port: u16, role: &str) -> String {
    format!(
        r#"{{"version":1,"peers":[{entry}]}}"#,
        entry = entry_for(address, port, role, CREDENTIAL)
    )
}

fn entry_for(address: &str, port: u16, role: &str, credential: &str) -> String {
    let socket = SocketAddr::new(address.parse::<IpAddr>().expect("ip"), port).to_string();
    format!(
        r#"{{"endpoint":{{"address":"{address}","port":{port}}},"paired":{{"address":"{socket}","host_name":"{address}","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"{fingerprint}"}},"secret":{{"role":"{role}","credential":"{credential}"}}}}"#,
        fingerprint = "ab".repeat(32)
    )
}

fn write_file(directory: &TempDir, contents: &str, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.path().join("mcp-peers.json");
    let mut file = std::fs::File::create(&path).expect("create peers file");
    file.write_all(contents.as_bytes())
        .expect("write peers file");
    file.flush().expect("flush");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

#[test]
fn a_private_file_matches_an_endpoint_exactly() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o600);
    let peers = Peers::load(&path).expect("load");

    let endpoint = RemoteEndpoint::new("127.0.0.1".parse().unwrap(), 7443);
    let peer = peers.find(&endpoint).expect("find");
    assert_eq!(peer.endpoint, endpoint);
    assert_eq!(peer.secret.role, PairingRole::Ai);
    assert_eq!(
        peer.paired.address,
        SocketAddr::from(([127, 0, 0, 1], 7443))
    );

    // The Debug form never prints the credential.
    let text = format!("{peer:?} {peers:?}");
    assert!(!text.contains(CREDENTIAL), "{text}");
}

#[test]
fn ipv6_endpoints_match_and_read_naturally() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("::1", 7000, "ai"), 0o600);
    let peers = Peers::load(&path).expect("load");
    let endpoint = RemoteEndpoint::new("::1".parse().unwrap(), 7000);
    assert!(peers.find(&endpoint).is_ok());
    assert_eq!(endpoint.to_string(), "[::1]:7000");

    // A different port or address is a different pairing.
    let wrong_port = RemoteEndpoint::new("::1".parse().unwrap(), 7001);
    let error = peers.find(&wrong_port).expect_err("no pairing");
    assert!(error.to_string().contains("[::1]:7001"), "{error}");
}

#[test]
fn client_local_scope_can_differ_from_the_pairing_host_scope() {
    let directory = TempDir::new().expect("tempdir");
    let contents = format!(
        r#"{{"version":1,"peers":[{{"endpoint":{{"address":"fe80::1","port":7331,"scope_id":3}},"paired":{{"address":"[fe80::1%2]:7331","host_name":"fe80::1","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"{}"}},"secret":{{"role":"ai","credential":"{}"}}}}]}}"#,
        "ab".repeat(32),
        CREDENTIAL,
    );
    let path = write_file(&directory, &contents, 0o600);
    let peers = Peers::load(&path).expect("load host pairing with client-local scope");
    let endpoint = RemoteEndpoint::with_scope("fe80::1".parse().unwrap(), 7331, 3);
    assert!(peers.find(&endpoint).is_ok());
}

#[test]
fn a_mapped_ipv6_endpoint_matches_its_ipv4_pairing() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o600);
    let peers = Peers::load(&path).expect("load");
    let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
    assert!(peers.find(&RemoteEndpoint::new(mapped, 7443)).is_ok());
}

#[test]
fn group_or_world_readable_files_are_refused() {
    let directory = TempDir::new().expect("tempdir");
    for mode in [0o644, 0o640, 0o604, 0o660] {
        let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), mode);
        let error = Peers::load(&path).expect_err("unsafe mode must be refused");
        let text = error.to_string();
        assert!(text.contains("permission"), "{text}");
        assert!(!text.contains(CREDENTIAL), "{text}");
    }
}

#[test]
fn an_executable_but_private_file_is_accepted() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o700);
    assert!(Peers::load(&path).is_ok());
}

#[test]
fn a_group_of_entries_matches_each_endpoint_unambiguously() {
    let directory = TempDir::new().expect("tempdir");
    let contents = format!(
        r#"{{"version":1,"peers":[{a},{b}]}}"#,
        a = entry_for("192.0.2.10", 7000, "ai", "Y3JlZGVudGlhbC1h"),
        b = entry_for("192.0.2.11", 7000, "ai", "Y3JlZGVudGlhbC1i")
    );
    let path = write_file(&directory, &contents, 0o600);
    let peers = Peers::load(&path).expect("load");
    assert_eq!(peers.len(), 2);
    let a = RemoteEndpoint::new("192.0.2.10".parse().unwrap(), 7000);
    let b = RemoteEndpoint::new("192.0.2.11".parse().unwrap(), 7000);
    assert_ne!(
        peers.find(&a).unwrap().secret.credential,
        peers.find(&b).unwrap().secret.credential
    );
}

#[test]
fn duplicate_endpoints_are_refused_as_ambiguous() {
    let directory = TempDir::new().expect("tempdir");
    let contents = format!(
        r#"{{"version":1,"peers":[{a},{b}]}}"#,
        a = entry_for("192.0.2.10", 7000, "ai", CREDENTIAL),
        b = entry_for("192.0.2.10", 7000, "ai", CREDENTIAL)
    );
    let path = write_file(&directory, &contents, 0o600);
    let error = Peers::load(&path).expect_err("duplicate must be refused");
    assert!(error.to_string().contains("192.0.2.10"), "{error}");
}

#[test]
fn malformed_and_mismatched_entries_are_refused_without_echoing_secrets() {
    let directory = TempDir::new().expect("tempdir");

    // Pairing address and endpoint disagree.
    let mismatched = format!(
        r#"{{"version":1,"peers":[{{"endpoint":{{"address":"192.0.2.10","port":7000}},"paired":{{"address":"192.0.2.11:7000","host_name":"192.0.2.10","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"{}"}},"secret":{{"role":"ai","credential":"{CREDENTIAL}"}}}}]}}"#,
        "ab".repeat(32)
    );
    let path = write_file(&directory, &mismatched, 0o600);
    let error = Peers::load(&path).expect_err("mismatch");
    let text = error.to_string();
    assert!(
        text.contains("192.0.2.11") || text.contains("192.0.2.10"),
        "{text}"
    );
    assert!(!text.contains(CREDENTIAL), "{text}");

    // The pairing TLS name is not the endpoint address.
    let wrong_name = format!(
        r#"{{"version":1,"peers":[{{"endpoint":{{"address":"192.0.2.10","port":7000}},"paired":{{"address":"192.0.2.10:7000","host_name":"192.0.2.99","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"{}"}},"secret":{{"role":"ai","credential":"{CREDENTIAL}"}}}}]}}"#,
        "ab".repeat(32)
    );
    let path = write_file(&directory, &wrong_name, 0o600);
    let error = Peers::load(&path).expect_err("wrong tls name");
    assert!(!error.to_string().contains(CREDENTIAL));

    // A truncated fingerprint is refused.
    let short = format!(
        r#"{{"version":1,"peers":[{{"endpoint":{{"address":"192.0.2.10","port":7000}},"paired":{{"address":"192.0.2.10:7000","host_name":"192.0.2.10","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"abcd"}},"secret":{{"role":"ai","credential":"{CREDENTIAL}"}}}}]}}"#
    );
    let path = write_file(&directory, &short, 0o600);
    assert!(Peers::load(&path).is_err());

    // Invalid JSON says so without containing the file's secret.
    let path = write_file(&directory, "{\"version\":1,\"peers\":[", 0o600);
    let error = Peers::load(&path).expect_err("invalid json");
    assert!(!error.to_string().contains(CREDENTIAL));

    // An unknown version is refused rather than guessed at.
    let path = write_file(
        &directory,
        &peer_json("192.0.2.10", 7000, "ai").replace("\"version\":1", "\"version\":9"),
        0o600,
    );
    assert!(Peers::load(&path).is_err());
}

#[test]
fn a_missing_file_has_a_clear_message_and_no_secret() {
    let directory = TempDir::new().expect("tempdir");
    let path = directory.path().join("absent.json");
    let error = Peers::load(&path).expect_err("missing");
    let text = error.to_string();
    assert!(text.contains("absent.json"), "{text}");
    assert!(text.contains("600"), "{text}");
}

#[test]
fn a_symbolic_link_is_refused() {
    let directory = TempDir::new().expect("tempdir");
    let real = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o600);
    let link = directory.path().join("link.json");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let error = Peers::load(&link).expect_err("symlink");
    assert!(error.to_string().contains("link.json"), "{error}");
}

#[test]
fn endpoint_validation_refuses_unusable_addresses() {
    for address in ["0.0.0.0", "::", "224.0.0.1", "255.255.255.255"] {
        let endpoint = RemoteEndpoint::new(address.parse().unwrap(), 7000);
        assert!(endpoint.validate().is_err(), "{address} must be refused");
    }
    let zero_port = RemoteEndpoint::new("127.0.0.1".parse().unwrap(), 0);
    assert!(zero_port.validate().is_err());
    for address in ["127.0.0.1", "::1", "192.0.2.10"] {
        let endpoint = RemoteEndpoint::new(address.parse().unwrap(), 7000);
        assert!(endpoint.validate().is_ok(), "{address} must be allowed");
    }
}

#[test]
fn role_is_matched_as_configured() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "human"), 0o600);
    let peers = Peers::load(&path).expect("load");
    let peer = peers
        .find(&RemoteEndpoint::new("127.0.0.1".parse().unwrap(), 7443))
        .unwrap();
    assert_eq!(peer.secret.role, PairingRole::Human);
}

#[test]
fn a_pairing_file_too_large_to_be_sane_is_refused() {
    let directory = TempDir::new().expect("tempdir");
    let mut text = r#"{"version":1,"peers":[]}"#.to_string();
    text.push_str(&" ".repeat(MAX_PEERS_FILE_BYTES));
    let path = write_file(&directory, &text, 0o600);
    let error = Peers::load(&path).expect_err("oversized file");
    assert!(error.to_string().contains("larger than"), "{error}");
}

#[test]
fn a_symbolic_link_says_it_is_not_a_regular_file() {
    let directory = TempDir::new().expect("tempdir");
    let real = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o600);
    let link = directory.path().join("link.json");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let error = Peers::load(&link).expect_err("symlink");
    assert!(matches!(error, PeerError::NotRegular { .. }), "{error}");
}

#[test]
fn a_symbolic_link_loop_in_a_parent_directory_is_an_io_error_not_a_link() {
    // The file itself is not a symbolic link and does not exist; O_NOFOLLOW
    // reports the unresolvable path as ELOOP. The message must describe the
    // path, not claim the (missing) file is a link or a device.
    let directory = TempDir::new().expect("tempdir");
    let looped = directory.path().join("selfloop");
    std::os::unix::fs::symlink(&looped, &looped).expect("symlink loop");
    let path = looped.join("peers.json");
    let error = Peers::load(&path).expect_err("unresolvable path");
    assert!(matches!(error, PeerError::Io { .. }), "{error}");
    assert!(!matches!(error, PeerError::NotRegular { .. }), "{error}");
    assert!(error.to_string().contains("peers.json"), "{error}");
}

mod saving {
    use super::*;
    use serial_nexus_client::{save_peer, PairedHost, PairingSecret, PeerSave};

    fn paired(address: &str, port: u16) -> PairedHost {
        PairedHost {
            address: SocketAddr::new(address.parse().expect("ip"), port),
            host_name: address.to_string(),
            ca_cert_der_b64: "Y2E=".to_string(),
            host_cert_fingerprint_sha256: "ab".repeat(32),
        }
    }

    fn secret(role: PairingRole, credential: &str) -> PairingSecret {
        PairingSecret {
            role,
            credential: credential.to_string(),
        }
    }

    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    fn names(directory: &TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory.path())
            .expect("read dir")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_new_endpoint_is_added_and_every_other_entry_is_kept() {
        let directory = TempDir::new().expect("tempdir");
        let existing = RemoteEndpoint::new("192.0.2.11".parse().unwrap(), 7000);
        let path = write_file(&directory, &peer_json("192.0.2.11", 7000, "ai"), 0o600);
        let added = RemoteEndpoint::new("192.0.2.10".parse().unwrap(), 7001);
        let outcome = save_peer(
            &path,
            added,
            paired("192.0.2.10", 7001),
            secret(PairingRole::Ai, "bmV3LWNyZWRlbnRpYWw"),
            false,
        )
        .expect("save");
        assert_eq!(outcome, PeerSave::Added);
        assert_eq!(mode_of(&path), 0o600, "the file stays owner-only");
        assert_eq!(names(&directory), vec!["mcp-peers.json"]);

        let peers = Peers::load(&path).expect("load");
        assert_eq!(peers.len(), 2);
        assert_eq!(
            peers.find(&existing).expect("old entry").secret.credential,
            CREDENTIAL,
            "the existing endpoint is untouched"
        );
        assert_eq!(
            peers.find(&added).expect("new entry").secret.credential,
            "bmV3LWNyZWRlbnRpYWw"
        );
    }

    #[test]
    fn a_missing_file_is_created_owner_only() {
        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("nested/serial-nexus/mcp-peers.json");
        let endpoint = RemoteEndpoint::new("127.0.0.1".parse().unwrap(), 7443);
        let outcome = save_peer(
            &path,
            endpoint,
            paired("127.0.0.1", 7443),
            secret(PairingRole::Ai, CREDENTIAL),
            false,
        )
        .expect("create");
        assert_eq!(outcome, PeerSave::Added);
        assert_eq!(mode_of(&path), 0o600);
        assert!(Peers::load(&path).expect("load").find(&endpoint).is_ok());
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("\"version\": 1"), "{text}");
    }

    #[test]
    fn an_existing_endpoint_needs_replace_and_leaves_the_file_alone_otherwise() {
        let directory = TempDir::new().expect("tempdir");
        let path = write_file(&directory, &peer_json("192.0.2.10", 7000, "ai"), 0o600);
        let before = std::fs::read(&path).expect("read");
        let endpoint = RemoteEndpoint::new("192.0.2.10".parse().unwrap(), 7000);
        let error = save_peer(
            &path,
            endpoint,
            paired("192.0.2.10", 7000),
            secret(PairingRole::Ai, "cmVwbGFjZW1lbnQ"),
            false,
        )
        .expect_err("no silent replacement");
        assert!(matches!(error, PeerError::Exists { .. }), "{error}");
        assert!(!error.to_string().contains(CREDENTIAL), "{error}");
        assert_eq!(std::fs::read(&path).expect("read"), before);

        assert_eq!(
            save_peer(
                &path,
                endpoint,
                paired("192.0.2.10", 7000),
                secret(PairingRole::Ai, "cmVwbGFjZW1lbnQ"),
                true,
            )
            .expect("replace"),
            PeerSave::Replaced
        );
        let peers = Peers::load(&path).expect("load");
        assert_eq!(
            peers.find(&endpoint).expect("entry").secret.credential,
            "cmVwbGFjZW1lbnQ"
        );
    }

    #[test]
    fn a_failed_save_changes_nothing_and_leaves_no_staging_file() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TempDir::new().expect("tempdir");
        let path = write_file(&directory, &peer_json("192.0.2.10", 7000, "ai"), 0o600);
        let before = std::fs::read(&path).expect("read");
        let mut locked = std::fs::Permissions::from_mode(0o500);
        std::fs::set_permissions(directory.path(), locked.clone()).expect("lock dir");
        let error = save_peer(
            &path,
            RemoteEndpoint::new("192.0.2.12".parse().unwrap(), 7000),
            paired("192.0.2.12", 7000),
            secret(PairingRole::Ai, "ZmFpbGVkLXNhdmU"),
            true,
        )
        .expect_err("the directory refuses the staging file");
        locked = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(directory.path(), locked).expect("unlock dir");

        assert!(!error.to_string().contains("ZmFpbGVkLXNhdmU"), "{error}");
        assert_eq!(std::fs::read(&path).expect("read"), before);
        assert_eq!(names(&directory), vec!["mcp-peers.json"]);
    }

    #[test]
    fn an_insecure_existing_file_or_a_symlink_is_refused_without_changes() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TempDir::new().expect("tempdir");
        let link = directory.path().join("mcp-peers.json");
        let target = directory.path().join("real.json");
        std::fs::write(&target, peer_json("127.0.0.1", 7443, "ai")).expect("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let before = std::fs::read(&target).expect("read");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let error = save_peer(
            &link,
            RemoteEndpoint::new("127.0.0.2".parse().unwrap(), 7443),
            paired("127.0.0.2", 7443),
            secret(PairingRole::Ai, "bGlua2Vk"),
            true,
        )
        .expect_err("a symlink is refused");
        assert!(matches!(error, PeerError::NotRegular { .. }), "{error}");
        assert_eq!(std::fs::read(&target).expect("read"), before);

        // A private target with unsafe mode is refused by the load path too.
        std::fs::remove_file(&link).expect("remove link");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let error = save_peer(
            &target,
            RemoteEndpoint::new("127.0.0.2".parse().unwrap(), 7443),
            paired("127.0.0.2", 7443),
            secret(PairingRole::Ai, "dW5zYWZl"),
            true,
        )
        .expect_err("an unsafe existing file is refused");
        assert!(error.to_string().contains("permission"), "{error}");
        assert_eq!(std::fs::read(&target).expect("read"), before);
    }

    #[test]
    fn a_wrong_role_or_mismatched_pairing_is_refused_before_writing() {
        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("mcp-peers.json");
        let error = save_peer(
            &path,
            RemoteEndpoint::new("127.0.0.1".parse().unwrap(), 7443),
            paired("127.0.0.2", 7443),
            secret(PairingRole::Ai, CREDENTIAL),
            false,
        )
        .expect_err("the pairing names another host");
        assert!(!error.to_string().contains(CREDENTIAL), "{error}");
        assert!(!path.exists(), "nothing was written");
        assert_eq!(names(&directory), Vec::<String>::new());
    }
}
