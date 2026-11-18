use clap::Parser;
use codex_anthropic_proxy::Args;

#[ctor::ctor]
fn pre_main() {
    codex_process_hardening::pre_main_hardening();
}

pub fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    codex_anthropic_proxy::run_main(args)
}
