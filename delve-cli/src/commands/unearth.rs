//! `delve unearth` — see the README's "CLI commands" section. The only command that touches
//! binary bytes; direct pass-through to the owning plugin's `fetch`, no
//! engine/EventBus/diff logic involved.

use std::path::PathBuf;

use delve_core::context::context_for_vendor;
use delve_core::plugin::{ArtifactSink, PluginRegistry};
use delve_core::store::MetadataStore;
use sha2::{Digest, Sha256};

use crate::cli::SelectorArgs;
use crate::config::Config;

/// Writes directly to a file on disk, hashing as it goes so the
/// post-download sha256 check (see the README's "CLI commands" section's
/// default-verify behavior) doesn't
/// need a second read pass over the file.
struct FileSink {
    file: std::fs::File,
    hasher: Sha256,
}

impl ArtifactSink for FileSink {
    fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.hasher.update(chunk);
        self.file.write_all(chunk)
    }

    fn finish(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.file.flush()
    }
}

impl FileSink {
    fn create(path: &std::path::Path) -> std::io::Result<Self> {
        Ok(Self {
            file: std::fs::File::create(path)?,
            hasher: Sha256::new(),
        })
    }

    /// Consumes the sink's hasher to produce the final digest. Takes `self`
    /// by value (not `&self`) because `Digest::finalize` does — matches
    /// the point in `run` where the sink is done being written to.
    fn finalize_sha256(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        // GenericArray<u8, U32> derefs to &[u8], so copy_from_slice works
        // directly rather than relying on a version-specific Into<[u8;32]>
        // conversion that may or may not exist for a given digest/generic-array
        // version pairing.
        out.copy_from_slice(&self.hasher.finalize());
        out
    }
}

pub async fn run(
    registry: &PluginRegistry,
    store: &dyn MetadataStore,
    config: &Config,
    selector: SelectorArgs,
    out: PathBuf,
    no_verify: bool,
) -> anyhow::Result<()> {
    let entry = store
        .resolve_one(&selector.into_store_selector())
        .await?
        .ok_or_else(|| anyhow::anyhow!("no matching firmware entry"))?
        .metadata;

    // FirmwareMetadata now carries vendor/device_family, so this works for
    // --id alone, not just natural-key selectors.
    let vendor_id = entry.vendor.clone();

    let plugin = registry
        .get(&vendor_id)
        .ok_or_else(|| anyhow::anyhow!("unknown vendor: {vendor_id}"))?;

    let (default_transport, overrides) = config.transport.resolve()?;
    let http_config = config.transport.http_client_config();
    let (default_rate_limit, rate_limit_overrides) = config.transport.rate_limits();
    let credentials = config.vendors.resolve_credentials(&vendor_id)?;
    let ctx = context_for_vendor(
        &default_transport,
        &overrides,
        &default_rate_limit,
        &rate_limit_overrides,
        &vendor_id,
        credentials,
        config.vendors.settings_for(&vendor_id),
        &http_config,
    )?;

    // FirmwareMetadata now carries vendor/device_family/source_url (see its
    // doc comment in model.rs), so a resolved entry is enough on its own to
    // rebuild the FirmwareRef fetch() needs — no separate lookup, and no
    // fabricated placeholder URL. `discovered_at` isn't part of stored
    // metadata (it's a discovery-time detail, not part of an entry's
    // identity or content), so it's set to "now" here — this FirmwareRef is
    // being reconstructed for a fetch happening now, not describing when
    // the entry was originally discovered.
    let source_ref = delve_core::model::FirmwareRef {
        vendor: entry.vendor.clone(),
        device_family: entry.device_family.clone(),
        source_url: entry.source_url.clone(),
        discovered_at: chrono::Utc::now(),
    };

    let mut sink = FileSink::create(&out)?;

    plugin.fetch(&ctx, &source_ref, &mut sink).await?;
    sink.finish()?;

    if !no_verify {
        let computed = sink.finalize_sha256();
        if let Some(expected) = entry.sha256 {
            if computed != expected {
                anyhow::bail!(
                    "sha256 mismatch after download — pass --no-verify to skip this check"
                );
            }
        }
    }

    println!(
        "Unearthed {} {} to {}",
        vendor_id,
        entry.version.raw,
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirms the real sha2 crate is actually wired up correctly — not
    /// just that it compiles, but that it produces the standard published
    /// test vector for SHA-256("abc"). Catches, e.g., a byte-order or
    /// truncation mistake in finalize_sha256's copy_from_slice that a
    /// "does it compile" check wouldn't.
    #[test]
    fn file_sink_computes_the_correct_sha256_for_a_known_test_vector() {
        let path =
            std::env::temp_dir().join(format!("delve-test-sink-{}.bin", uuid::Uuid::new_v4()));
        let mut sink = FileSink::create(&path).expect("temp file should be creatable");

        sink.write_chunk(b"abc").unwrap();
        sink.finish().unwrap();

        let computed = sink.finalize_sha256();
        let hex: String = computed.iter().map(|b| format!("{:02x}", b)).collect();

        // Standard published test vector: SHA-256("abc")
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_sink_computes_the_correct_sha256_for_empty_input() {
        let path =
            std::env::temp_dir().join(format!("delve-test-sink-{}.bin", uuid::Uuid::new_v4()));
        let mut sink = FileSink::create(&path).expect("temp file should be creatable");

        sink.finish().unwrap(); // no write_chunk calls at all

        let computed = sink.finalize_sha256();
        let hex: String = computed.iter().map(|b| format!("{:02x}", b)).collect();

        // Standard published test vector: SHA-256("")
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_sink_hashes_multiple_chunks_as_one_contiguous_stream() {
        // A real download arrives in many chunks — the hash must be over
        // the concatenation of all of them, not just the last one written,
        // and must match hashing the same bytes in one shot.
        let path =
            std::env::temp_dir().join(format!("delve-test-sink-{}.bin", uuid::Uuid::new_v4()));
        let mut sink = FileSink::create(&path).expect("temp file should be creatable");

        sink.write_chunk(b"ab").unwrap();
        sink.write_chunk(b"cd").unwrap();
        sink.write_chunk(b"ef").unwrap();
        sink.finish().unwrap();

        let computed = sink.finalize_sha256();

        let mut one_shot = Sha256::new();
        one_shot.update(b"abcdef");
        let mut expected = [0u8; 32];
        expected.copy_from_slice(&one_shot.finalize());

        assert_eq!(computed, expected);

        let _ = std::fs::remove_file(&path);
    }
}
