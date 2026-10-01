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
    /// Override the default config path (~/.config/delve/config.toml) — see
    /// the README's "Configuration reference" section.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scrape enabled vendors. Auto-detects baseline vs. incremental per
    /// vendor (see the README's "Baseline vs incremental digs" section) —
    /// no flag needed to say which.
    Dig {
        /// Restrict to one vendor. Omit to dig every enabled vendor.
        #[arg(long)]
        vendor: Option<String>,

        /// Clear the vendor's baseline first, so this dig is treated as a
        /// fresh baseline (silent, no notifications) — see the README's
        /// "Baseline vs incremental digs" and "CLI commands" sections.
        #[arg(long, requires = "vendor")]
        redig: bool,

        /// Dev escape hatch: run the named vendor even though its plugin
        /// has `tos_reviewed: false`, logging a loud warning. Only valid
        /// with `--vendor`, so an unfiltered (e.g. scheduled) dig can never
        /// pick it up. Results are written to the real store — see the
        /// README's "Compliance" section.
        #[arg(long, requires = "vendor")]
        allow_unreviewed: bool,
    },

    /// Print currently-known firmware attributes. No downloading — see the
    /// README's "CLI commands" section.
    Catalog {
        #[command(flatten)]
        selector: SelectorArgs,

        /// Print every field instead of just the most useful ones.
        #[arg(long, short = 'l', alias = "verbose")]
        long: bool,
    },

    /// Show the revision history for one firmware entry — see the README's
    /// "CLI commands" section. Requires
    /// enough of `selector` to resolve to exactly one entry.
    Provenance {
        #[command(flatten)]
        selector: SelectorArgs,
    },

    /// Download a firmware binary. Never happens automatically during
    /// `dig` — this is the only command that touches binary bytes (see the
    /// README's "CLI commands" section).
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

/// Shared selector flags across `catalog`/`provenance`/`unearth` (see the
/// README's "CLI commands" section).
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

    /// Resolve to the highest-precedence version for the matched entries,
    /// per VersionKey ordering (see the README's "Data model and identity
    /// keys" section) — only meaningful when a reliable
    /// ordinal exists; entries with an Opaque version scheme can't be
    /// resolved this way.
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
