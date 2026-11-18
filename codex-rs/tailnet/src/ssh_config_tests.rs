use super::*;
use pretty_assertions::assert_eq;

fn host(alias: &str, hostname: Option<&str>, port: Option<u16>) -> SshHost {
    SshHost {
        alias: alias.to_string(),
        hostname: hostname.map(str::to_string),
        port,
    }
}

#[test]
fn collects_concrete_aliases_and_skips_patterns() {
    let config = r#"
# personal machines
Host devbox build-box
    HostName devbox.tail1234.ts.net
    User me

Host *.internal !bastion.internal
    ProxyJump bastion

Host bastion.internal
  Port=2222

Host "quoted alias" plain\ space
    HostName 10.0.0.5

Host gpu?
    HostName gpu.example.com

Host *
    ServerAliveInterval 30
"#;
    assert_eq!(
        parse_ssh_config(config),
        vec![
            host("bastion.internal", None, Some(2222)),
            host("build-box", Some("devbox.tail1234.ts.net"), None),
            host("devbox", Some("devbox.tail1234.ts.net"), None),
            host("plain space", Some("10.0.0.5"), None),
            host("quoted alias", Some("10.0.0.5"), None),
        ]
    );
}

#[test]
fn first_matching_value_wins_and_percent_h_expands() {
    let config = r#"
Host web
    HostName first.example.com
Host web
    HostName second.example.com
Host api
Host *
    HostName %h.corp.example.com
    Port 2200
Host tokenized
    HostName %r.example.com
"#;
    assert_eq!(
        parse_ssh_config(config),
        vec![
            host("api", Some("api.corp.example.com"), Some(2200)),
            // The `Host *` HostName comes first for this alias, so it wins.
            host("tokenized", Some("tokenized.corp.example.com"), Some(2200)),
            host("web", Some("first.example.com"), Some(2200)),
        ]
    );
}

#[test]
fn match_blocks_make_values_unknown() {
    let config = r#"
Host laptop
Match exec "test -f /tmp/x"
    HostName elsewhere.example.com
Host laptop
    HostName laptop.example.com
"#;
    assert_eq!(parse_ssh_config(config), vec![host("laptop", None, None)]);
}

#[test]
fn unsupported_tokens_leave_hostname_unknown() {
    let config = "Host weird\n    HostName %r@%h\n";
    assert_eq!(parse_ssh_config(config), vec![host("weird", None, None)]);
}

#[test]
fn unbalanced_quotes_do_not_add_aliases() {
    let config = "Host \"broken\n    HostName x\nHost ok\n";
    assert_eq!(parse_ssh_config(config), vec![host("ok", None, None)]);
}

#[test]
fn follows_includes_relative_to_ssh_dir_with_globs() -> io::Result<()> {
    let home = tempfile::tempdir()?;
    let ssh_dir = home.path().join(".ssh");
    std::fs::create_dir_all(ssh_dir.join("config.d"))?;
    std::fs::write(
        ssh_dir.join("config"),
        "Include config.d/*.conf ~/.ssh/extra\nHost main\n    HostName main.example.com\n",
    )?;
    std::fs::write(
        ssh_dir.join("config.d/10-work.conf"),
        "Host work\n    HostName work.example.com\n    Port 2022\n",
    )?;
    std::fs::write(ssh_dir.join("config.d/.hidden.conf"), "Host hidden\n")?;
    std::fs::write(ssh_dir.join("config.d/notes.txt"), "Host notes\n")?;
    std::fs::write(ssh_dir.join("extra"), "Host extra\nInclude config\n")?;

    let hosts = SshConfigSource::for_home(home.path().to_path_buf()).hosts()?;
    assert_eq!(
        hosts,
        vec![
            host("extra", None, None),
            host("main", Some("main.example.com"), None),
            host("work", Some("work.example.com"), Some(2022)),
        ]
    );
    Ok(())
}

#[test]
fn missing_config_yields_no_hosts() -> io::Result<()> {
    let home = tempfile::tempdir()?;
    assert_eq!(
        SshConfigSource::for_home(home.path().to_path_buf()).hosts()?,
        Vec::new()
    );
    Ok(())
}
