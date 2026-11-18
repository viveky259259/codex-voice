//! The TUI's own session layer against the Claude Code backend and a fake
//! `claude` (python3 -I). Never runs the real CLI.

use super::*;
use crate::agent_backend::ClaudeCodeSelection;
use crate::legacy_core::config::ConfigBuilder;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_voice_commands::VoiceCommand;
use codex_voice_commands::VoiceContext;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;

struct Fake {
    dir: tempfile::TempDir,
    session: AppServerSession,
    config: Config,
}

impl Fake {
    async fn start(config_model: Option<&str>) -> Option<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = codex_claude_agent_backend::test_support::write_fake_claude(dir.path())?;
        let mut config = ConfigBuilder::default()
            .codex_home(dir.path().to_path_buf())
            .build()
            .await
            .expect("config");
        config.model = config_model.map(str::to_string);
        let selection = ClaudeCodeSelection { claude_bin: script };
        let mut options = crate::agent_backend::claude_code_options(&selection, &config);
        options.cwd = dir.path().to_path_buf();
        options.env = vec![
            (
                "FAKE_CLAUDE_LOG".into(),
                dir.path().join("log.jsonl").into(),
            ),
            ("FAKE_CLAUDE_SCENARIO".into(), "text".into()),
        ];
        let session = AppServerSession::new(
            crate::backend_id::BackendId::new(),
            codex_claude_agent_backend::start(options),
            ThreadParamsMode::Embedded,
        );
        Some(Self {
            dir,
            session,
            config,
        })
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("log line"))
            .collect()
    }

    async fn wait_for_set_model(&self, model: &Value) -> Value {
        for _ in 0..200 {
            if let Some(found) = self.log().into_iter().find_map(|entry| {
                let stdin = entry.get("stdin")?;
                (stdin["request"]["subtype"] == "set_model" && stdin["request"]["model"] == *model)
                    .then(|| stdin.clone())
            }) {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no set_model {model} in {:#?}", self.log());
    }
}

#[test]
fn only_claude_models_reach_the_cli() {
    let selection = ClaudeCodeSelection {
        claude_bin: "claude".into(),
    };
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = runtime
        .block_on(
            ConfigBuilder::default()
                .codex_home(dir.path().to_path_buf())
                .build(),
        )
        .expect("config");
    config.model = Some("gpt-5.1".to_string());
    assert_eq!(
        crate::agent_backend::claude_code_options(&selection, &config).model,
        None
    );
    config.model = Some("claude-opus-5-5".to_string());
    assert_eq!(
        crate::agent_backend::claude_code_options(&selection, &config)
            .model
            .as_deref(),
        Some("claude-opus-5-5")
    );
}

#[tokio::test]
async fn bootstrap_lists_claude_models_and_voice_and_model_picker_switch_with_set_model() {
    let Some(mut fake) = Fake::start(/*config_model*/ None).await else {
        eprintln!("python3 not found; skipping");
        return;
    };
    let config = fake.config.clone();
    let bootstrap = fake.session.bootstrap(&config).await.expect("bootstrap");
    assert!(!bootstrap.requires_openai_auth);
    let models: Vec<(&str, &str)> = bootstrap
        .available_models
        .iter()
        .map(|preset| (preset.model.as_str(), preset.display_name.as_str()))
        .collect();
    assert_eq!(
        models,
        vec![
            ("default", "Default (recommended)"),
            ("opus", "Opus"),
            ("sonnet", "Sonnet"),
            ("claude-fable-5-2", "Claude Fable 5.2"),
            ("claude-haiku-5-5", "Claude Haiku 5.5"),
        ]
    );
    assert_eq!(bootstrap.default_model.as_deref(), Some("default"));

    let started = fake
        .session
        .start_thread(&config)
        .await
        .expect("thread/start");
    assert_eq!(started.session.model, "default");
    assert_eq!(started.session.model_provider_id, "claude-code");
    let thread_id = started.session.thread_id;

    // "switch to fable" resolves against the Claude list by its family name.
    let context = VoiceContext {
        known_models: crate::app::voice_command_backend::voice_models_for(
            &bootstrap.available_models,
            /*claude_code*/ true,
        ),
        ..Default::default()
    };
    let VoiceCommand::SwitchModel { slug } =
        codex_voice_commands::parse_voice_command("switch to fable", &context)
    else {
        panic!("not a model switch");
    };
    assert_eq!(slug, "claude-fable-5-2");
    let VoiceCommand::SwitchModel { slug: opus } =
        codex_voice_commands::parse_voice_command("use opus", &context)
    else {
        panic!("not a model switch");
    };
    assert_eq!(opus, "opus");

    // Both the spoken switch and the /model picker apply through
    // thread/settings/update (App::sync_active_thread_model_setting).
    for (picked, sent) in [
        (slug, json!("claude-fable-5-2")),
        ("sonnet".to_string(), json!("sonnet")),
    ] {
        let updated = fake
            .session
            .thread_settings_update(ThreadSettingsUpdateParams {
                thread_id: thread_id.to_string(),
                model: Some(picked),
                ..Default::default()
            })
            .await
            .expect("thread/settings/update");
        assert!(updated);
        assert_eq!(
            fake.wait_for_set_model(&sent).await["request"],
            json!({"subtype": "set_model", "model": sent})
        );
    }
    let argvs: Vec<Value> = fake
        .log()
        .into_iter()
        .filter_map(|entry| entry.get("argv").cloned())
        .collect();
    assert_eq!(argvs.len(), 1, "switching must not restart the session");
}

#[tokio::test]
async fn a_configured_claude_model_is_passed_as_model() {
    let Some(mut fake) = Fake::start(Some("claude-sonnet-5-5")).await else {
        return;
    };
    let config = fake.config.clone();
    fake.session.bootstrap(&config).await.expect("bootstrap");
    let started = fake
        .session
        .start_thread(&config)
        .await
        .expect("thread/start");
    assert_eq!(started.session.model, "claude-sonnet-5-5");
    let argv = fake
        .log()
        .into_iter()
        .find_map(|entry| entry.get("argv").cloned())
        .expect("argv");
    assert!(
        argv.as_array()
            .expect("array")
            .contains(&json!("--model=claude-sonnet-5-5")),
        "{argv}"
    );
}

#[test]
fn claude_code_sessions_are_titled_as_claude_code() {
    let title = |claude_code| {
        crate::history_cell::agent_title_spans("1.2.3", claude_code)
            .iter()
            .map(|span| span.content.to_string())
            .collect::<String>()
    };
    assert_eq!(title(true), ">_ Claude Code (codex-voice v1.2.3)");
    assert_eq!(title(false), ">_ OpenAI Codex (v1.2.3)");
    assert_eq!(crate::agent_backend::spoken_agent_name(true), "Claude Code");
}

#[test]
fn claude_code_decision_cells_name_claude_code() {
    use crate::history_cell::ApprovalDecisionActor;
    use crate::history_cell::ApprovalDecisionSubject;
    use crate::history_cell::ReviewDecision;
    let text = |decision, agent: &str| {
        crate::history_cell::approval_decision_cell_for_agent(
            ApprovalDecisionSubject::Command(vec!["rm".to_string(), "hello.txt".to_string()]),
            decision,
            ApprovalDecisionActor::User,
            agent,
        )
        .display_lines(/*width*/ 100)
        .iter()
        .map(ToString::to_string)
        .collect::<String>()
    };
    let agent = crate::agent_backend::approval_agent_name(/*claude_code*/ true);
    assert_eq!(
        text(ReviewDecision::Denied, agent),
        "✗ You did not approve Claude Code to run rm hello.txt"
    );
    assert_eq!(
        text(ReviewDecision::Approved, agent),
        "✔ You approved Claude Code to run rm hello.txt this time"
    );
    assert_eq!(
        text(
            ReviewDecision::Denied,
            crate::agent_backend::approval_agent_name(/*claude_code*/ false)
        ),
        "✗ You did not approve codex to run rm hello.txt"
    );
}

#[test]
fn claude_code_status_permissions_are_claude_codes() {
    assert_eq!(
        crate::agent_backend::status_permissions(Some("claude-code")).as_deref(),
        Some("Claude Code (asks before risky tools)")
    );
    assert_eq!(
        crate::agent_backend::status_permissions(Some("openai")),
        None
    );
}
