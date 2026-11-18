use super::*;
use codex_tailnet::MachineSource;
use pretty_assertions::assert_eq;

fn machine(name: &str, online: Option<bool>, is_self: bool) -> Machine {
    Machine {
        name: name.to_string(),
        hostname: name.to_string(),
        dns_name: Some(format!("{name}.tail1234.ts.net")),
        ips: vec!["100.64.0.2".to_string()],
        online,
        os: Some("linux".to_string()),
        is_self,
        ssh_enabled: Some(name == "devbox"),
        ssh_alias: None,
        ssh_destination: format!("{name}.tail1234.ts.net"),
        source: MachineSource::Tailscale,
    }
}

#[test]
fn ssh_address_accepts_bare_hosts_and_urls() {
    assert_eq!(
        ssh_address("devbox", None).expect("bare host"),
        "ssh://devbox"
    );
    assert_eq!(
        ssh_address("me@devbox.tail1234.ts.net", Some("/opt/codex".to_string()))
            .expect("user@host"),
        "ssh://me@devbox.tail1234.ts.net?codex_bin=%2Fopt%2Fcodex"
    );
    assert_eq!(
        ssh_address("ssh://me@devbox:2222", Some(String::new())).expect("url"),
        "ssh://me@devbox:2222"
    );
    assert!(ssh_address("-oProxyCommand=x", None).is_err());
}

#[test]
fn picker_offers_online_peers_and_retries_bad_input() {
    let machines = vec![
        machine("devbox", Some(true), false),
        machine("laptop", Some(true), true),
        machine("pi", None, false),
        machine("old", Some(false), false),
    ];
    let mut input = "9\nnope\n2\n".as_bytes();
    let mut output = Vec::new();
    let picked = pick_machine(&machines, &mut input, &mut output).expect("pick");
    assert_eq!(picked.name, "pi");
    let output = String::from_utf8(output).expect("utf8");
    assert_eq!(
        output,
        "Machines:\n   1) devbox -> devbox.tail1234.ts.net (linux, tailscale ssh)\n   2) pi -> pi.tail1234.ts.net (linux, ssh config)\nAttach to [1-2]: Enter a number between 1 and 2, or q to cancel.\nAttach to [1-2]: Enter a number between 1 and 2, or q to cancel.\nAttach to [1-2]: "
    );
}

#[test]
fn picker_reports_no_candidates_and_cancellation() {
    let offline = vec![machine("old", Some(false), false)];
    let err = pick_machine(&offline, &mut "1\n".as_bytes(), &mut Vec::new())
        .expect_err("nothing to pick");
    assert!(err.to_string().contains("no online machines"), "{err}");

    let online = vec![machine("devbox", Some(true), false)];
    let err = pick_machine(&online, &mut "q\n".as_bytes(), &mut Vec::new()).expect_err("cancelled");
    assert!(err.to_string().contains("no machine selected"), "{err}");
}

#[test]
fn table_lists_status_and_destination() {
    let table = format_machine_table(&[
        machine("devbox", Some(true), false),
        machine("laptop", Some(true), true),
        machine("old", Some(false), false),
    ]);
    assert_eq!(
        table,
        "NAME    STATUS        OS     SSH        DESTINATION\n\
         devbox  online        linux  tailscale  devbox.tail1234.ts.net\n\
         laptop  this machine  linux  -          laptop.tail1234.ts.net\n\
         old     offline       linux  -          old.tail1234.ts.net\n\
         \nAttach with `codex remote attach <DESTINATION>`, or run `codex remote attach` to pick.\n"
    );
}
