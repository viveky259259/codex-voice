use super::*;
use pretty_assertions::assert_eq;

/// Trimmed from a real `tailscale status --json` (1.7x) with MagicDNS on,
/// one online Linux peer running Tailscale SSH, one offline macOS peer, and a
/// Windows peer that reports no `Online` field.
const RUNNING: &str = r#"{
  "Version": "1.76.6-t1234abcd",
  "TUN": true,
  "BackendState": "Running",
  "HaveNodeKey": true,
  "AuthURL": "",
  "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"],
  "Self": {
    "ID": "nSELF",
    "PublicKey": "nodekey:self",
    "HostName": "laptop",
    "DNSName": "laptop.tail1234.ts.net.",
    "OS": "macOS",
    "UserID": 1,
    "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"],
    "Online": true,
    "Active": false,
    "CapMap": {"https": null}
  },
  "Health": [],
  "MagicDNSSuffix": "tail1234.ts.net",
  "CurrentTailnet": {
    "Name": "user@example.com",
    "MagicDNSSuffix": "tail1234.ts.net",
    "MagicDNSEnabled": true
  },
  "Peer": {
    "nodekey:aaa": {
      "ID": "nAAA",
      "HostName": "devbox",
      "DNSName": "devbox.tail1234.ts.net.",
      "OS": "linux",
      "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"],
      "Online": true,
      "Active": true,
      "sshHostKeys": ["ssh-ed25519 AAAAC3Nza devbox"]
    },
    "nodekey:bbb": {
      "ID": "nBBB",
      "HostName": "Old-MacBook",
      "DNSName": "renamed-mac.tail1234.ts.net.",
      "OS": "macOS",
      "TailscaleIPs": ["100.64.0.3"],
      "Online": false,
      "LastSeen": "2026-10-01T10:00:00Z"
    },
    "nodekey:ccc": {
      "ID": "nCCC",
      "HostName": "gaming-pc",
      "DNSName": "gaming-pc.tail1234.ts.net.",
      "OS": "windows",
      "TailscaleIPs": ["100.64.0.4"]
    }
  },
  "User": {"1": {"ID": 1, "LoginName": "user@example.com"}}
}"#;

const NEEDS_LOGIN: &str = r#"{
  "Version": "1.76.6",
  "BackendState": "NeedsLogin",
  "AuthURL": "",
  "TailscaleIPs": null,
  "Self": {
    "ID": "",
    "HostName": "laptop",
    "DNSName": "",
    "OS": "linux",
    "TailscaleIPs": null,
    "Online": false
  },
  "Health": ["You are logged out."],
  "MagicDNSSuffix": "",
  "CurrentTailnet": null,
  "Peer": null,
  "User": null
}"#;

const STOPPED: &str = r#"{
  "BackendState": "Stopped",
  "Self": {
    "HostName": "laptop",
    "DNSName": "laptop.tail1234.ts.net.",
    "OS": "linux",
    "TailscaleIPs": ["100.101.102.103"],
    "Online": false
  },
  "CurrentTailnet": {"MagicDNSEnabled": true},
  "Peer": {
    "nodekey:aaa": {
      "HostName": "devbox",
      "DNSName": "devbox.tail1234.ts.net.",
      "OS": "linux",
      "TailscaleIPs": ["100.64.0.2"],
      "Online": true
    }
  }
}"#;

#[test]
fn parses_running_status_with_peers() {
    let status = parse_status(RUNNING).expect("status should parse");
    assert_eq!(status.backend_state.as_deref(), Some("Running"));
    let machines = status.into_machines().expect("running tailnet is usable");
    assert_eq!(
        machines,
        vec![
            Machine {
                name: "laptop".to_string(),
                hostname: "laptop".to_string(),
                dns_name: Some("laptop.tail1234.ts.net".to_string()),
                ips: vec![
                    "100.101.102.103".to_string(),
                    "fd7a:115c:a1e0::1".to_string()
                ],
                online: Some(true),
                os: Some("macOS".to_string()),
                is_self: true,
                ssh_enabled: Some(false),
                ssh_alias: None,
                ssh_destination: "laptop.tail1234.ts.net".to_string(),
                source: MachineSource::Tailscale,
            },
            Machine {
                name: "devbox".to_string(),
                hostname: "devbox".to_string(),
                dns_name: Some("devbox.tail1234.ts.net".to_string()),
                ips: vec!["100.64.0.2".to_string(), "fd7a:115c:a1e0::2".to_string()],
                online: Some(true),
                os: Some("linux".to_string()),
                is_self: false,
                ssh_enabled: Some(true),
                ssh_alias: None,
                ssh_destination: "devbox.tail1234.ts.net".to_string(),
                source: MachineSource::Tailscale,
            },
            Machine {
                name: "renamed-mac".to_string(),
                hostname: "Old-MacBook".to_string(),
                dns_name: Some("renamed-mac.tail1234.ts.net".to_string()),
                ips: vec!["100.64.0.3".to_string()],
                online: Some(false),
                os: Some("macOS".to_string()),
                is_self: false,
                ssh_enabled: Some(false),
                ssh_alias: None,
                ssh_destination: "renamed-mac.tail1234.ts.net".to_string(),
                source: MachineSource::Tailscale,
            },
            Machine {
                name: "gaming-pc".to_string(),
                hostname: "gaming-pc".to_string(),
                dns_name: Some("gaming-pc.tail1234.ts.net".to_string()),
                ips: vec!["100.64.0.4".to_string()],
                online: Some(false),
                os: Some("windows".to_string()),
                is_self: false,
                ssh_enabled: Some(false),
                ssh_alias: None,
                ssh_destination: "gaming-pc.tail1234.ts.net".to_string(),
                source: MachineSource::Tailscale,
            },
        ]
    );
}

#[test]
fn needs_login_is_reported_as_not_logged_in() {
    let status = parse_status(NEEDS_LOGIN).expect("logged-out status should parse");
    assert_eq!(status.machines.len(), 1);
    assert!(!status.machines[0].online.unwrap_or(true));
    let err = status
        .into_machines()
        .expect_err("logged-out tailnet is unusable");
    assert!(
        matches!(&err, TailnetError::NotLoggedIn { state } if state == "NeedsLogin"),
        "{err:?}"
    );
    assert!(err.to_string().contains("tailscale login"));
}

#[test]
fn stopped_backend_marks_everything_offline_and_errors() {
    let status = parse_status(STOPPED).expect("stopped status should parse");
    assert!(
        status
            .machines
            .iter()
            .all(|machine| machine.online == Some(false)),
        "a stopped backend keeps a stale netmap; nothing is reachable"
    );
    let err = status
        .into_machines()
        .expect_err("stopped tailnet is unusable");
    assert!(
        matches!(&err, TailnetError::NotRunning { state } if state == "Stopped"),
        "{err:?}"
    );
    assert!(err.to_string().contains("tailscale up"));
}

#[test]
fn magic_dns_disabled_uses_ipv4_destination() {
    let raw = r#"{
      "BackendState": "Running",
      "CurrentTailnet": {"MagicDNSEnabled": false},
      "Peer": {
        "k": {
          "HostName": "devbox",
          "DNSName": "devbox.tail1234.ts.net.",
          "TailscaleIPs": ["fd7a:115c:a1e0::2", "100.64.0.2"],
          "Online": true
        }
      }
    }"#;
    let machines = parse_status(raw)
        .and_then(TailscaleStatus::into_machines)
        .expect("status should parse");
    assert_eq!(
        machines[0].dns_name.as_deref(),
        Some("devbox.tail1234.ts.net")
    );
    assert_eq!(machines[0].ssh_destination, "100.64.0.2");
}

#[test]
fn missing_backend_state_trusts_the_peer_list() {
    let raw =
        r#"{"Peer": {"k": {"HostName": "box", "TailscaleIPs": ["100.64.0.9"], "Online": true}}}"#;
    let machines = parse_status(raw)
        .and_then(TailscaleStatus::into_machines)
        .expect("status should parse");
    assert_eq!(machines[0].name, "box");
    assert_eq!(machines[0].dns_name, None);
    assert_eq!(machines[0].online, Some(true));
}

#[test]
fn invalid_json_is_a_parse_error() {
    let err = parse_status("tailscale: command output").expect_err("not JSON");
    assert!(matches!(err, TailnetError::Parse(_)), "{err:?}");
}

#[tokio::test]
async fn missing_binary_is_reported_as_not_installed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("no-such-tailscale");
    let err = TailscaleCli::with_binary(&missing)
        .machines()
        .await
        .expect_err("missing binary");
    assert!(matches!(err, TailnetError::NotInstalled { .. }), "{err:?}");
    assert!(err.to_string().contains("not installed"));
}

#[cfg(unix)]
#[tokio::test]
async fn runs_the_cli_and_maps_daemon_errors() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let write_script = |name: &str, body: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        path
    };
    let fixture = dir.path().join("status.json");
    std::fs::write(&fixture, RUNNING).expect("write fixture");
    let ok = write_script(
        "tailscale-ok",
        &format!(
            "[ \"$1 $2\" = \"status --json\" ] || exit 64\ncat '{}'",
            fixture.display()
        ),
    );
    let machines = TailscaleCli::with_binary(&ok)
        .machines()
        .await
        .expect("fake CLI should succeed");
    assert_eq!(machines.len(), 4);

    let down = write_script(
        "tailscale-down",
        "echo 'failed to connect to local tailscaled; it doesn'\\''t appear to be running (sudo systemctl start tailscaled ?)' >&2\nexit 1",
    );
    let err = TailscaleCli::with_binary(&down)
        .machines()
        .await
        .expect_err("daemon down");
    assert!(
        matches!(err, TailnetError::DaemonUnavailable { .. }),
        "{err:?}"
    );

    let leaky = write_script(
        "tailscale-leaky",
        "echo 'bad key tskey-auth-SECRET' >&2\nexit 2",
    );
    let err = TailscaleCli::with_binary(&leaky)
        .machines()
        .await
        .expect_err("command failed");
    let message = err.to_string();
    assert!(!message.contains("SECRET"), "{message}");
    assert!(message.contains("[redacted]"), "{message}");
}
