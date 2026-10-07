//! CLI surface. See the README's "CLI commands" section for the full
//! reference: `dig`, `catalog`, `provenance`, `unearth`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "delve",
    version,
    about = "Embedded firmware scraper and notification framework"
)]
pub struct Cli {
    /// Override the default config path (~/.config/delve/config.toml).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Show timestamps in UTC instead of in the local timezone. Use it in
    /// scripts that parse the output.
    #[arg(long, global = true)]
    pub utc: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scrape enabled vendors. A vendor's first dig is a silent baseline (no
    /// notifications); later digs report what changed. No flag needed to say
    /// which.
    Dig {
        /// Restrict to one vendor. Omit to dig every enabled vendor.
        #[arg(long)]
        vendor: Option<String>,

        /// Clear the vendor's baseline first, so this dig is treated as a
        /// fresh baseline (silent, no notifications).
        #[arg(long, requires = "vendor")]
        redig: bool,

        /// Dev escape hatch: run the named vendor even though its plugin
        /// has `tos_reviewed: false` (its terms of service and robots.txt
        /// haven't been reviewed), logging a loud warning. Only valid with
        /// `--vendor`, so an unfiltered (e.g. scheduled) dig can never pick
        /// it up. Results are written to the real store.
        #[arg(long, requires = "vendor")]
        allow_unreviewed: bool,
    },

    /// Print currently-known firmware attributes. No downloading.
    Catalog {
        #[command(flatten)]
        selector: SelectorArgs,

        /// Print every field instead of just the most useful ones.
        #[arg(long, short = 'l', alias = "verbose")]
        long: bool,
    },

    /// Show the revision history for one firmware entry. Requires enough of
    /// the selector flags to resolve to exactly one entry.
    Provenance {
        #[command(flatten)]
        selector: SelectorArgs,
    },

    /// Ask questions and write the config file, starting from the current one
    /// when there is one. Needs a terminal, unless `--defaults` is given.
    #[command(alias = "init", alias = "configure")]
    Survey {
        /// Write the file to stdout instead of to the config path.
        #[arg(long)]
        print: bool,

        /// Ask nothing: write the commented file with every default.
        #[arg(long)]
        defaults: bool,
    },

    /// Download a firmware binary. Never happens automatically during
    /// `dig` — this is the only command that touches binary bytes.
    Unearth {
        #[command(flatten)]
        selector: SelectorArgs,

        /// Directory or file path to write the downloaded artifact to.
        #[arg(long)]
        out: PathBuf,

        /// Skip sha256 verification against the stored hash after download.
        #[arg(long)]
        no_verify: bool,
    },
}

/// Shared selector flags across `catalog`/`provenance`/`unearth`.
/// `--id` is mutually sufficient on its own; the rest compose the natural key.
#[derive(Args, Clone)]
pub struct SelectorArgs {
    /// Resolve directly by the entry's surrogate id, bypassing every other
    /// selector flag.
    #[arg(long)]
    pub id: Option<uuid::Uuid>,

    #[arg(long)]
    pub vendor: Option<String>,

    #[arg(long)]
    pub device_family: Option<String>,

    /// May be repeated for multi-target hardware keys.
    #[arg(long = "hardware")]
    pub hardware: Vec<String>,

    #[arg(long, conflicts_with = "latest")]
    pub version: Option<String>,

    /// Resolve to the newest version for the matched entries, by version
    /// number rather than release date (a patch backported to an older
    /// branch doesn't count as newer). Only meaningful when the versions can
    /// be ordered; entries with an opaque version scheme can't be resolved
    /// this way.
    #[arg(long, conflicts_with = "version")]
    pub latest: bool,
}

impl SelectorArgs {
    pub fn into_store_selector(self) -> delve_core::store::FirmwareSelector {
        delve_core::store::FirmwareSelector {
            id: self.id,
            vendor: self.vendor,
            device_family: self.device_family,
            hardware: if self.hardware.is_empty() {
                None
            } else {
                Some(self.hardware)
            },
            version: self.version,
            latest: self.latest,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    /// The binary can be installed without the README, so no help text may
    /// send the reader there.
    fn assert_no_readme(cmd: &mut clap::Command, path: &str) {
        let help = cmd.render_long_help().to_string();
        assert!(
            !help.contains("README"),
            "`{path} --help` mentions the README:\n{help}"
        );
        let names: Vec<String> = cmd
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for name in names {
            let sub = cmd.find_subcommand_mut(&name).expect("listed above");
            assert_no_readme(sub, &format!("{path} {name}"));
        }
    }

    #[test]
    fn help_text_never_mentions_the_readme() {
        let mut cmd = Cli::command();
        // `render_long_help` needs the command built, with its global flags.
        cmd.build();
        assert_no_readme(&mut cmd, "delve");
    }
}
