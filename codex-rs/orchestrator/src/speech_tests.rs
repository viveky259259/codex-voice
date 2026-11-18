use super::activity_phrase;
use super::agent_line;
use super::elapsed_phrase;
use super::spoken_machine;
use super::spoken_message;
use crate::ActivityItem;
use crate::AgentInfo;
use crate::AgentStatus;
use crate::ItemStatus;
use crate::test_support::agent;
use pretty_assertions::assert_eq;

fn command(command: &str, status: ItemStatus) -> ActivityItem {
    ActivityItem::Command {
        command: command.to_string(),
        status,
        exit_code: None,
        output: Some("lots of output".to_string()),
    }
}

#[test]
fn agent_lines_read_like_a_status_board() {
    let now = 10_000;
    let running = AgentInfo {
        turn_started_at: Some(now - 130),
        latest: Some(command(
            "bash -lc \"cargo test -p codex-core\"",
            ItemStatus::InProgress,
        )),
        ..agent("t", "build", AgentStatus::Running)
    };
    let waiting = AgentInfo {
        latest: Some(ActivityItem::FileChange {
            paths: vec!["src/lib.rs".to_string()],
            status: ItemStatus::InProgress,
        }),
        ..agent("t", "tests", AgentStatus::WaitingForApproval)
    };
    let lines = [
        agent_line("desktop", &running, /*latest*/ None, now),
        agent_line("mac-studio", &waiting, /*latest*/ None, now),
        agent_line(
            "laptop",
            &agent("t", "docs", AgentStatus::Idle),
            /*latest*/ None,
            now,
        ),
        agent_line(
            "laptop",
            &agent("t", "x", AgentStatus::WaitingForInput),
            /*latest*/ None,
            now,
        ),
        agent_line(
            "laptop",
            &agent("t", "y", AgentStatus::Error),
            /*latest*/ None,
            now,
        ),
        agent_line(
            "laptop",
            &agent("t", "z", AgentStatus::Running),
            /*latest*/ None,
            now,
        ),
    ];
    assert_eq!(
        lines,
        [
            "build on desktop: running cargo test codex-core, 2 minutes",
            "tests on mac studio: waiting for your approval to edit lib.rs",
            "docs on laptop: idle",
            "x on laptop: waiting for your input",
            "y on laptop: hit an error",
            "z on laptop: working",
        ]
    );
}

#[test]
fn activity_phrases_never_include_output() {
    let phrases = [
        activity_phrase(&command("cargo test", ItemStatus::Completed)),
        activity_phrase(&command("./scripts/check.sh --fast", ItemStatus::Failed)),
        activity_phrase(&ActivityItem::FileChange {
            paths: vec!["a.rs".to_string(), "b.rs".to_string()],
            status: ItemStatus::Completed,
        }),
        activity_phrase(&ActivityItem::ToolCall {
            tool: "search".to_string(),
            status: ItemStatus::InProgress,
        }),
        activity_phrase(&ActivityItem::Reasoning {
            summary: "secret plan".to_string(),
        }),
    ];
    assert_eq!(
        phrases,
        [
            "ran cargo test",
            "check.sh failed",
            "edited two files",
            "calling search",
            "thinking",
        ]
    );
}

#[test]
fn messages_skip_code_and_diffs() {
    assert_eq!(
        spoken_message("**Done.** All `47` tests pass now, and the parser handles every case."),
        Some("Done. All 47 tests pass now, and the parser handles…".to_string())
    );
    assert_eq!(spoken_message("```rust\nfn main() {}\n```"), None);
    assert_eq!(spoken_message("diff --git a/x b/x"), None);
    assert_eq!(spoken_message("   \n  "), None);
}

#[test]
fn elapsed_and_machine_names() {
    assert_eq!(
        [
            elapsed_phrase(100, 130),
            elapsed_phrase(0, 60),
            elapsed_phrase(0, 7300),
            elapsed_phrase(500, 100),
        ],
        ["just started", "1 minute", "2 hours", "just started"]
    );
    assert_eq!(spoken_machine("MacStudio"), "Mac Studio");
    assert_eq!(spoken_machine("work_box"), "work box");
}
