//! `codex-local-voice`: try the local voice engine without the TUI.

use std::path::PathBuf;

use codex_local_voice::LocalVoice;
use codex_local_voice::LocalVoiceConfig;
use codex_local_voice::LocalVoiceEvent;
use codex_local_voice::mcp::McpClient;
use tokio::io::AsyncBufReadExt;

const USAGE: &str = "usage:
  codex-local-voice config                      print the default config as TOML
  codex-local-voice tools [MCP_URL]             list an MCP server's tools and schemas
  codex-local-voice say [--config FILE] TEXT    speak TEXT and exit
  codex-local-voice listen [--config FILE]      print voice events; typed lines are spoken

Set RUST_LOG=codex_local_voice=debug for details.";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(message) = run(std::env::args().skip(1).collect()).await {
        eprintln!("{message}");
        std::process::exit(1);
    }
    // The engine has shut down and reaped its children. Exit now: returning
    // would make the runtime wait for the blocking stdin read `listen` may
    // still have pending, which only ends at EOF.
    std::process::exit(0);
}

async fn run(args: Vec<String>) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else {
        return Err(USAGE.to_string());
    };
    match command.as_str() {
        "config" => {
            let text = toml::to_string_pretty(&LocalVoiceConfig::default())
                .map_err(|err| err.to_string())?;
            println!("{text}");
            Ok(())
        }
        "tools" => {
            let url = rest
                .first()
                .cloned()
                .unwrap_or_else(|| codex_local_voice::DEFAULT_MCP_URL.to_string());
            let client = McpClient::new(url).map_err(|err| err.to_string())?;
            let tools = client.tools().await.map_err(|err| err.to_string())?;
            for tool in tools {
                println!("{}", tool.name);
                if let Some(description) = tool.description {
                    println!("  {}", description.trim());
                }
                println!("  input schema: {}", tool.input_schema);
            }
            Ok(())
        }
        "say" => {
            let (config, text) = parse_config(rest)?;
            if text.is_empty() {
                return Err(USAGE.to_string());
            }
            let mut config = config;
            config.capture.enabled = false;
            let (voice, mut events) = LocalVoice::start(config);
            voice.speak(text.join(" "));
            while let Some(event) = events.recv().await {
                println!("{event:?}");
                match event {
                    LocalVoiceEvent::SpeakingFinished => break,
                    LocalVoiceEvent::Error { .. } => {
                        voice.shutdown().await;
                        return Err("speaking failed".to_string());
                    }
                    _ => {}
                }
            }
            voice.shutdown().await;
            Ok(())
        }
        "listen" => {
            let (config, _) = parse_config(rest)?;
            let (voice, mut events) = LocalVoice::start(config);
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Some(event) => println!("{event:?}"),
                        None => break,
                    },
                    line = lines.next_line() => match line {
                        Ok(Some(line)) if line.trim() == "/stop" => voice.stop_speaking(),
                        Ok(Some(line)) => voice.speak(line),
                        _ => break,
                    },
                    () = shutdown_signal() => break,
                }
            }
            voice.shutdown().await;
            Ok(())
        }
        _ => Err(USAGE.to_string()),
    }
}

/// Ctrl-C or, on Unix, SIGTERM: either shuts the engine down cleanly, so its
/// capture, model and player processes (each in its own process group) stop
/// too.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::SignalKind;
        use tokio::signal::unix::signal;
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn parse_config(args: &[String]) -> Result<(LocalVoiceConfig, Vec<String>), String> {
    let mut config_path: Option<PathBuf> = None;
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--config" {
            config_path = Some(PathBuf::from(iter.next().ok_or("--config needs a file")?));
        } else {
            rest.push(arg.clone());
        }
    }
    let config = match config_path {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|err| format!("{}: {err}", path.display()))?;
            LocalVoiceConfig::from_toml_str(&text).map_err(|err| err.to_string())?
        }
        None => LocalVoiceConfig::default(),
    };
    Ok((config, rest))
}
