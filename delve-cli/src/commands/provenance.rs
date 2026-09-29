//! `delve provenance` — see the README's "CLI commands" section.

use delve_core::store::{FirmwareKey, MetadataStore};

use crate::cli::SelectorArgs;

pub async fn run(store: &dyn MetadataStore, selector: SelectorArgs) -> anyhow::Result<()> {
    // provenance requires enough specificity to resolve to exactly one
    // entry (see the README's "CLI commands" section) — resolve_one enforces
    // that, returning StoreError::Ambiguous
    // if the selector was too broad.
    let entry = store
        .resolve_one(&selector.into_store_selector())
        .await?
        .ok_or_else(|| anyhow::anyhow!("no matching firmware entry"))?;

    // FirmwareMetadata now carries vendor/device_family itself (see its doc
    // comment in model.rs), so --id alone is enough to build the key —
    // no separate --vendor/--device-family required.
    let key = FirmwareKey::from_metadata(&entry);
    let revisions = store.history(&key).await?;

    println!("{:<24} {:<38} {}", "OBSERVED_AT", "RUN_ID", "VERSION / HASH");
    for rev in &revisions {
        let hash = rev
            .metadata
            .sha256
            .map(|h| h.iter().map(|b| format!("{:02x}", b)).collect::<String>())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<24} {:<38} {} / {}",
            rev.observed_at.to_rfc3339(),
            rev.run_id,
            rev.metadata.version.raw,
            &hash[..hash.len().min(12)],
        );
    }

    Ok(())
}
