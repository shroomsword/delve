mod cli;
mod commands;
mod config;

use clap::Parser;
use cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let config = config::load(cli.config.as_deref())?;

    let store = delve_store_sqlite::SqliteStore::open(&config.database_path).await?;
    let registry = delve_core::plugin::PluginRegistry::discover();

    match cli.command {
        Command::Dig { vendor, redig } => commands::dig::run(&config, &registry, &store, vendor, redig).await,
        Command::Catalog { selector, long } => commands::catalog::run(&store, selector, long).await,
        Command::Provenance { selector } => commands::provenance::run(&store, selector).await,
        Command::Unearth { selector, out, no_verify } => {
            commands::unearth::run(&registry, &store, &config, selector, out, no_verify).await
        }
    }
}
