mod cli;
mod commands;
mod config;

use clap::Parser;
use cli::{Cli, Command};

// Vendor crates register themselves with `inventory::submit!`, but nothing
// else in the CLI names them, and rustc doesn't link a dependency that's
// never referenced — so without these the plugins silently never register
// and `dig --vendor <id>` reports "unknown vendor".
#[cfg(feature = "vendor-cisco")]
use vendor_cisco as _;
#[cfg(feature = "vendor-unifi")]
use vendor_unifi as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    // Before loading the config: survey writes it, and can replace one that
    // doesn't parse.
    if let Command::Survey { print, defaults } = cli.command {
        return commands::survey::run(cli.config.as_deref(), print, defaults).await;
    }
    let config = config::load(cli.config.as_deref())?;

    let store = delve_store_sqlite::SqliteStore::open(&config.database_path).await?;
    let registry = delve_core::plugin::PluginRegistry::discover();

    match cli.command {
        Command::Dig {
            vendor,
            redig,
            allow_unreviewed,
        } => commands::dig::run(&config, &registry, &store, vendor, redig, allow_unreviewed).await,
        Command::Catalog { selector, long } => {
            commands::catalog::run(&store, selector, long, &mut std::io::stdout().lock()).await
        }
        Command::Provenance { selector } => {
            let time = commands::timestamp::Display::from_utc_flag(cli.utc);
            commands::provenance::run(&store, selector, time, &mut std::io::stdout().lock()).await
        }
        Command::Unearth {
            selector,
            out,
            no_verify,
        } => commands::unearth::run(&registry, &store, &config, selector, out, no_verify).await,
        Command::Survey { .. } => unreachable!("survey ran before the config was loaded"),
    }
}
