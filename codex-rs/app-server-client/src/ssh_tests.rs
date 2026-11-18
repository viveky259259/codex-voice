use super::*;
use pretty_assertions::assert_eq;

fn target(addr: &str) -> SshTarget {
    SshTarget::parse_url(addr).expect("valid ssh address")
}

#[test]
fn ssh_args_are_hardened_and_end_options_before_the_host() {
    assert_eq!(
        target("ssh://me@devbox:2222").ssh_args("codex", &["app-server", "proxy"]),
        vec![
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "RemoteCommand=none",
            "-o",
            "RequestTTY=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-p",
            "2222",
            "-l",
            "me",
            "--",
            "devbox",
            "codex app-server proxy",
        ]
    );
}

#[test]
fn remote_codex_bin_is_quoted_for_the_remote_shell() {
    let args =
        target("ssh://devbox").ssh_args("/opt/my codex/it's/codex", &["app-server", "proxy"]);
    assert_eq!(
        args.last().map(String::as_str),
        Some(r"'/opt/my codex/it'\''s/codex' app-server proxy")
    );
    let args = target("ssh://devbox").ssh_args("~/.codex/bin/codex", &["app-server", "proxy"]);
    assert_eq!(
        args.last().map(String::as_str),
        Some("~/.codex/bin/codex app-server proxy")
    );
    assert_eq!(shell_quote("$(touch pwned)"), "'$(touch pwned)'");
    assert_eq!(shell_quote(""), "''");
}

#[test]
fn display_url_round_trips_user_port_and_codex_bin() {
    let target = SshTarget {
        user: Some("first.last@corp".to_string()),
        host: "fd7a:115c:a1e0::2".to_string(),
        port: Some(2200),
        remote_codex_bin: Some("/opt/codex dev/codex".to_string()),
        ssh_program: None,
    };
    let url = target.display_url();
    assert_eq!(
        url,
        "ssh://first.last%40corp@[fd7a:115c:a1e0::2]:2200?codex_bin=%2Fopt%2Fcodex+dev%2Fcodex"
    );
    assert_eq!(SshTarget::parse_url(&url), Ok(target));
}

#[tokio::test]
async fn missing_ssh_client_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut target = target("ssh://devbox?codex_bin=codex");
    target.ssh_program = Some(dir.path().join("no-such-ssh"));
    let err = connect_ssh_endpoint(&target)
        .await
        .err()
        .expect("missing ssh should fail");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert!(err.to_string().contains("install OpenSSH"), "{err}");
}
