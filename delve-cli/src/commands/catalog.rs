//! `delve catalog` — see the README's "CLI commands" section.

use delve_core::store::MetadataStore;

use crate::cli::SelectorArgs;

pub async fn run(store: &dyn MetadataStore, selector: SelectorArgs, long: bool) -> anyhow::Result<()> {
    let entries = store.resolve_many(&selector.into_store_selector()).await?;

    if entries.is_empty() {
        println!("No matching firmware entries.");
        return Ok(());
    }

    if long {
        for e in &entries {
            println!("vendor:         {}", e.vendor);
            println!("device_family:  {}", e.device_family);
            println!("source_url:     {}", e.source_url);
            println!("version:        {}", e.version.raw);
            println!("hardware:       {}", e.hardware_targets.join(", "));
            println!("release_date:   {:?}", e.release_date);
            println!(
                "sha256:         {}",
                e.sha256.map(|h| hex_string(&h)).unwrap_or_else(|| "-".into())
            );
            println!(
                "release_notes:  {}",
                e.release_notes_url.as_ref().map(|u| u.as_str()).unwrap_or("-")
            );
            println!(
                "signature:      {}",
                e.signature
                    .as_ref()
                    .map(|s| format!("{} (verified: {})", s.scheme, s.verified))
                    .unwrap_or_else(|| "-".into())
            );
            println!("---");
        }
    } else {
        // Default view: the most useful fields only, one line per entry.
        println!(
            "{:<12} {:<14} {:<24} {:<16} {:<12} {}",
            "VENDOR", "DEVICE_FAMILY", "VERSION", "HARDWARE", "RELEASED", "SHA256 (short)"
        );
        for e in &entries {
            let short_hash = e.sha256.map(|h| hex_string(&h)[..12].to_string()).unwrap_or_else(|| "-".into());
            println!(
                "{:<12} {:<14} {:<24} {:<16} {:<12} {}",
                e.vendor,
                e.device_family,
                e.version.raw,
                e.hardware_targets.join("+"),
                e.release_date.map(|d| d.to_string()).unwrap_or_else(|| "-".into()),
                short_hash,
            );
        }
    }

    Ok(())
}

fn hex_string(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
