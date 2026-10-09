mod cli;
mod commands;
mod config;
mod report;

use std::process::ExitCode;

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

/// Stdout for output that may be colored. The commands always write their
/// styles; this strips them unless `--color` and the environment say a
/// terminal wants them (`auto` honours `NO_COLOR`, `TERM=dumb` and whether
/// stdout is a terminal; `always` does not).
///
/// Wraps `Stdout`, which locks for each write, and not a held `StdoutLock`:
/// the commands keep this stream across `.await`s while they query the store,
/// and a lock held that long blocks anything else that writes to stdout, such
/// as a log line from another thread, which then never lets the query finish.
fn stdout(choice: clap::ColorChoice) -> anstream::AutoStream<std::io::Stdout> {
    let choice = match choice {
        clap::ColorChoice::Auto => anstream::ColorChoice::Auto,
        clap::ColorChoice::Always => anstream::ColorChoice::Always,
        clap::ColorChoice::Never => anstream::ColorChoice::Never,
    };
    anstream::AutoStream::new(std::io::stdout(), choice)
}

#[tokio::main]
async fn main() -> ExitCode {
    // Before logging starts: `--utc` picks the log clock, and a usage error
    // is clap's to print and exit on.
    let cli = Cli::parse();
    let clock = report::Clock::new(cli.utc);
    // Logs go to stderr, as with other command-line tools: stdout is for the
    // result of the command, so `delve catalog | grep ...` stays clean at any
    // `RUST_LOG`. (The subscriber's default is stdout.)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_timer(clock)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report::write_fatal(&clock, &err);
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
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
            commands::catalog::run(&store, selector, long, &mut stdout(cli.color)).await
        }
        Command::Provenance { selector } => {
            let time = commands::timestamp::Display::from_utc_flag(cli.utc);
            commands::provenance::run(&store, selector, time, &mut stdout(cli.color)).await
        }
        Command::Unearth {
            selector,
            out,
            no_verify,
        } => commands::unearth::run(&registry, &store, &config, selector, out, no_verify).await,
        Command::Survey { .. } => unreachable!("survey ran before the config was loaded"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::stdout;

    /// A command holds this stream across its `.await`s, so another thread
    /// must still be able to write to stdout while it exists (#76).
    #[test]
    fn the_output_stream_does_not_hold_the_stdout_lock() {
        let _out = stdout(clap::ColorChoice::Never);
        let (done, locked) = mpsc::channel();
        std::thread::spawn(move || {
            let _lock = std::io::stdout().lock();
            let _ = done.send(());
        });
        locked
            .recv_timeout(Duration::from_secs(5))
            .expect("stdout stayed locked while the output stream existed");
    }
}
