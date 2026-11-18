//! Localhost proxy that lets Codex talk to Anthropic Claude models.
//!
//! Codex speaks the OpenAI Responses API. This proxy accepts
//! `POST /v1/responses` (streaming), translates the request into an Anthropic
//! Messages request, and translates the Messages event stream back into
//! Responses events. Point Codex at it with a custom model provider whose
//! `base_url` is `http://127.0.0.1:<port>/v1`.

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod dump;
mod reasoning;
mod request;
mod server;
mod stream;
mod tools;

pub use request::TranslateOptions;
pub use server::ApiKey;
pub use server::DEFAULT_ANTHROPIC_BASE_URL;
pub use server::DEFAULT_ANTHROPIC_VERSION;
pub use server::ProxyConfig;
pub use server::router;
pub use server::serve;

/// Port used when `--port` is not given.
pub const DEFAULT_PORT: u16 = 18765;

/// Environment variable holding the Anthropic API key.
pub const API_KEY_ENV_VAR: &str = "ANTHROPIC_API_KEY";

#[derive(Clone, Debug, Parser)]
#[command(
    name = "codex-anthropic-proxy",
    about = "Serve the OpenAI Responses API on localhost, backed by Anthropic Claude models"
)]
pub struct Args {
    /// Address to listen on. Keep this on loopback: the proxy adds your API key.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// Port to listen on (0 picks an ephemeral port).
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// Write `{"port": <u16>, "pid": <u32>}` to this file once listening.
    #[arg(long, value_name = "FILE")]
    pub server_info: Option<PathBuf>,

    /// Anthropic API base URL; requests go to `<URL>/v1/messages`.
    #[arg(long, default_value = DEFAULT_ANTHROPIC_BASE_URL)]
    pub anthropic_base_url: String,

    /// Value of the `anthropic-version` header.
    #[arg(long, default_value = DEFAULT_ANTHROPIC_VERSION)]
    pub anthropic_version: String,

    /// Optional `anthropic-beta` header value (comma-separated betas).
    #[arg(long)]
    pub anthropic_beta: Option<String>,

    /// `max_tokens` for each Anthropic request.
    #[arg(long, default_value_t = TranslateOptions::default().max_tokens)]
    pub max_tokens: u32,

    /// Record each translated request and both streams into this directory.
    #[arg(long, value_name = "DIR")]
    pub dump_dir: Option<PathBuf>,
}

#[derive(Serialize)]
struct ServerInfo {
    port: u16,
    pid: u32,
}

/// Entry point for the binary.
pub fn run_main(args: Args) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    codex_utils_rustls_provider::ensure_rustls_crypto_provider();

    let api_key = std::env::var(API_KEY_ENV_VAR)
        .ok()
        .filter(|key| !key.trim().is_empty())
        .with_context(|| format!("{API_KEY_ENV_VAR} must be set to an Anthropic API key"))?;
    let config = ProxyConfig {
        anthropic_base_url: args.anthropic_base_url,
        api_key: ApiKey::new(api_key),
        anthropic_version: args.anthropic_version,
        anthropic_beta: args.anthropic_beta,
        translate: TranslateOptions {
            max_tokens: args.max_tokens,
        },
        dump_dir: args.dump_dir,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = TcpListener::bind((args.host, args.port))
            .await
            .with_context(|| format!("binding {}:{}", args.host, args.port))?;
        let addr = listener.local_addr()?;
        if let Some(path) = &args.server_info {
            let info = ServerInfo {
                port: addr.port(),
                pid: std::process::id(),
            };
            let mut line = serde_json::to_string(&info)?;
            line.push('\n');
            tokio::fs::write(path, line)
                .await
                .with_context(|| format!("writing {}", path.display()))?;
        }
        info!(
            "codex-anthropic-proxy listening on http://{addr}/v1 (upstream {})",
            config.anthropic_base_url
        );
        serve(listener, config).await
    })
}
