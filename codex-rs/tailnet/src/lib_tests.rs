use super::*;
use pretty_assertions::assert_eq;

fn tailnet_machine(name: &str, online: bool, is_self: bool) -> Machine {
    Machine {
        name: name.to_string(),
        hostname: name.to_string(),
        dns_name: Some(format!("{name}.tail1234.ts.net")),
        ips: vec![format!("100.64.0.{}", name.len())],
        online: Some(online),
        os: Some("linux".to_string()),
        is_self,
        ssh_enabled: Some(false),
        ssh_alias: None,
        ssh_destination: format!("{name}.tail1234.ts.net"),
        source: MachineSource::Tailscale,
    }
}

struct FixedTailnet(Result<Vec<Machine>, fn() -> TailnetError>);

impl TailnetSource for FixedTailnet {
    async fn machines(&self) -> Result<Vec<Machine>, TailnetError> {
        match &self.0 {
            Ok(machines) => Ok(machines.clone()),
            Err(make_error) => Err(make_error()),
        }
    }
}

#[test]
fn merge_attaches_matching_aliases_dedupes_and_sorts_online_first() {
    let offline = tailnet_machine("old", false, false);
    let me = tailnet_machine("me", true, true);
    let devbox = tailnet_machine("devbox", true, false);
    let duplicate_devbox = tailnet_machine("devbox", true, false);
    let merged = merge_machines(
        vec![offline, me, devbox, duplicate_devbox],
        vec![
            SshHost {
                alias: "work".to_string(),
                hostname: Some("devbox.tail1234.ts.net".to_string()),
                port: None,
            },
            SshHost {
                alias: "pi".to_string(),
                hostname: Some("192.168.1.20".to_string()),
                port: Some(2222),
            },
        ],
    );
    let summary = merged
        .iter()
        .map(|machine| {
            (
                machine.name.as_str(),
                machine.online,
                machine.ssh_destination.as_str(),
                machine.source,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        vec![
            ("devbox", Some(true), "work", MachineSource::Tailscale),
            (
                "me",
                Some(true),
                "me.tail1234.ts.net",
                MachineSource::Tailscale
            ),
            ("pi", None, "pi", MachineSource::SshConfig),
            (
                "old",
                Some(false),
                "old.tail1234.ts.net",
                MachineSource::Tailscale
            ),
        ]
    );
    assert_eq!(merged[0].ssh_alias.as_deref(), Some("work"));
}

#[tokio::test]
async fn discover_falls_back_to_ssh_config_with_a_warning() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::create_dir_all(home.path().join(".ssh"))?;
    std::fs::write(home.path().join(".ssh/config"), "Host devbox\n")?;
    let ssh_config = SshConfigSource::for_home(home.path().to_path_buf());
    let tailnet = FixedTailnet(Err(|| TailnetError::NotInstalled {
        tried: "tailscale".to_string(),
    }));

    let discovery = discover(&tailnet, Some(&ssh_config))
        .await
        .expect("ssh config hosts are enough");
    assert_eq!(discovery.machines.len(), 1);
    assert_eq!(discovery.machines[0].ssh_destination, "devbox");
    assert_eq!(discovery.warnings.len(), 1);
    assert!(discovery.warnings[0].contains("not installed"));

    let err = discover(&tailnet, None)
        .await
        .expect_err("nothing discovered");
    assert!(matches!(err, TailnetError::NotInstalled { .. }), "{err:?}");
    Ok(())
}

#[tokio::test]
async fn discover_returns_tailnet_machines() {
    let tailnet = FixedTailnet(Ok(vec![
        tailnet_machine("zeta", true, false),
        tailnet_machine("alpha", false, false),
    ]));
    let discovery = discover(&tailnet, None).await.expect("discovery");
    assert_eq!(
        discovery
            .machines
            .iter()
            .map(|machine| machine.name.as_str())
            .collect::<Vec<_>>(),
        vec!["zeta", "alpha"]
    );
    assert!(discovery.warnings.is_empty());
}

#[test]
fn machine_json_uses_camel_case() {
    let json = serde_json::to_value(tailnet_machine("devbox", true, false)).expect("serialize");
    assert_eq!(json["dnsName"], "devbox.tail1234.ts.net");
    assert_eq!(json["sshDestination"], "devbox.tail1234.ts.net");
    assert_eq!(json["source"], "tailscale");
    assert_eq!(json["isSelf"], false);
}
