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
///
/// Either a file path, opened at once, or a directory, in which the file is
/// opened on its first write (or on `finish`, for an empty download) so the
/// plugin can first suggest a name for it with `suggest_file_name`.
struct FileSink {
    file: Option<std::fs::File>,
    /// The file's path once it is open.
    path: Option<PathBuf>,
    /// For a directory target: the directory, the name used when the plugin
    /// suggests none that is usable, and the plugin's suggestion.
    directory: Option<DirectoryTarget>,
    hasher: Sha256,
}

struct DirectoryTarget {
    dir: PathBuf,
    fallback_name: String,
    suggested_name: Option<String>,
}

impl ArtifactSink for FileSink {
    fn suggest_file_name(&mut self, name: &str) {
        if let Some(directory) = &mut self.directory {
            if is_plain_file_name(name) {
                directory.suggested_name = Some(name.to_string());
            }
        }
    }

    fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.hasher.update(chunk);
        self.open()?.write_all(chunk)
    }

    fn finish(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.open()?.flush()
    }
}

impl FileSink {
    /// A sink writing to the file at `path`, created at once.
    fn create(path: &std::path::Path) -> std::io::Result<Self> {
        Ok(Self {
            file: Some(std::fs::File::create(path)?),
            path: Some(path.to_path_buf()),
            directory: None,
            hasher: Sha256::new(),
        })
    }

    /// A sink writing a file inside `dir`, which is created if needed. The
    /// file is named by the plugin's suggestion if it makes a usable one,
    /// otherwise `fallback_name`.
    fn in_directory(dir: PathBuf, fallback_name: String) -> Self {
        Self {
            file: None,
            path: None,
            directory: Some(DirectoryTarget {
                dir,
                fallback_name,
                suggested_name: None,
            }),
            hasher: Sha256::new(),
        }
    }

    /// The open file, creating the directory and the file first if this is
    /// the first call for a directory target.
    fn open(&mut self) -> std::io::Result<&mut std::fs::File> {
        if self.file.is_none() {
            if let Some(directory) = &self.directory {
                std::fs::create_dir_all(&directory.dir)?;
                let name = directory
                    .suggested_name
                    .as_deref()
                    .unwrap_or(&directory.fallback_name);
                let path = directory.dir.join(name);
                self.file = Some(std::fs::File::create(&path)?);
                self.path = Some(path);
            }
        }
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("no output file"))
    }

    /// Where the file was written, once it is open.
    fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
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

/// Whether a plugin's suggested name is safe to use as a file name inside
/// the output directory: not empty, not `.` or `..`, and no path separator
/// or control character that could place the file somewhere else.
fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| std::path::is_separator(c) || c.is_control())
}

/// Whether `--out` names a directory: an existing directory, or a path
/// ending in a path separator (which is created if needed). Any other path
/// is the file itself.
fn names_a_directory(out: &std::path::Path) -> bool {
    out.is_dir()
        || out
            .as_os_str()
            .to_string_lossy()
            .chars()
            .next_back()
            .is_some_and(std::path::is_separator)
}

/// The file name used in a directory when the plugin suggests none:
/// `<vendor>-<hardware>-<version>.bin`, with anything that isn't safe in a
/// file name, such as the `+` in UniFi versions, replaced by `_`.
fn fallback_file_name(entry: &delve_core::model::FirmwareMetadata) -> String {
    let raw = format!(
        "{}-{}-{}.bin",
        entry.vendor,
        entry.hardware_targets.join("+"),
        entry.version.raw
    );
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
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

    let mut sink = if names_a_directory(&out) {
        FileSink::in_directory(out, fallback_file_name(&entry))
    } else {
        FileSink::create(&out)?
    };

    plugin.fetch(&ctx, &source_ref, &mut sink).await?;
    sink.finish()?;
    let written = sink
        .path()
        .ok_or_else(|| anyhow::anyhow!("the download produced no output file"))?
        .to_path_buf();

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
        written.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `unearth` resolves its selector the same way `catalog` does, so every
    /// flag ignores case. The mock plugin cannot download, so a selector that
    /// resolved gets as far as the fetch and fails there, while one that
    /// matched nothing fails before it.
    #[tokio::test]
    async fn every_selector_flag_ignores_case() {
        use crate::commands::test_support::*;

        let store = memory_store().await;
        seed(
            &store,
            MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]),
        )
        .await;
        let registry =
            PluginRegistry::from_plugins(vec![Box::new(MockPlugin::new("acme", vec![]))]);
        let out = std::env::temp_dir().join(format!("delve-unearth-{}", uuid::Uuid::new_v4()));

        let attempt = |f: fn(&mut SelectorArgs)| {
            let (registry, store, out) = (&registry, &store, out.clone());
            async move {
                let mut s = selector();
                f(&mut s);
                run(registry, store, &config(""), s, out, false)
                    .await
                    .unwrap_err()
                    .to_string()
            }
        };

        let matched = [
            attempt(|s| s.vendor = Some("ACME".into())).await,
            attempt(|s| s.device_family = Some("WIDGET".into())).await,
            attempt(|s| s.hardware = vec!["Rev-A".into()]).await,
            attempt(|s| s.version = Some("1.0".into())).await,
            attempt(|s| {
                s.vendor = Some("Acme".into());
                s.device_family = Some("Widget".into());
                s.hardware = vec!["REV-A".into()];
                s.version = Some("1.0".into());
            })
            .await,
        ];
        for err in matched {
            assert_ne!(
                err, "no matching firmware entry",
                "the selector should match"
            );
        }
        let missed = attempt(|s| s.vendor = Some("ACMEE".into())).await;
        assert_eq!(missed, "no matching firmware entry");
        let _ = std::fs::remove_file(&out);
    }

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

    fn entry(
        vendor: &str,
        hardware: &[&str],
        version: &str,
    ) -> delve_core::model::FirmwareMetadata {
        delve_core::model::FirmwareMetadata {
            vendor: vendor.to_string(),
            device_family: "USW".to_string(),
            source_url: "https://example.com/firmware/1".parse().unwrap(),
            version: delve_core::model::VersionKey::opaque(version.to_string()),
            release_date: None,
            sha256: None,
            signature: None,
            hardware_targets: hardware.iter().map(|h| h.to_string()).collect(),
            release_notes_url: None,
            display_name: None,
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("delve-test-out-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn a_file_path_is_not_a_directory_and_an_existing_directory_or_trailing_separator_is() {
        assert!(!names_a_directory(std::path::Path::new(
            "some/dir/mini.bin"
        )));
        assert!(names_a_directory(std::path::Path::new("./downloads/")));
        let dir = temp_dir();
        std::fs::create_dir(&dir).unwrap();
        assert!(names_a_directory(&dir));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn the_fallback_name_is_safe_for_odd_versions_and_several_targets() {
        let e = entry("unifi", &["USMINI"], "v1.6.3+574");
        // `+` is not safe in every file system's names, so it is replaced.
        assert_eq!(fallback_file_name(&e), "unifi-USMINI-v1.6.3_574.bin");
        let e = entry("v/x", &["A", "B"], "1 2/3");
        assert_eq!(fallback_file_name(&e), "v_x-A_B-1_2_3.bin");
    }

    #[test]
    fn a_suggested_name_is_used_inside_a_directory_that_is_created() {
        let dir = temp_dir().join("nested");
        let mut sink = FileSink::in_directory(dir.clone(), "fallback.bin".into());
        sink.suggest_file_name("U7PG2-6.8.2.bin");
        sink.write_chunk(b"abc").unwrap();
        sink.finish().unwrap();

        assert_eq!(sink.path(), Some(dir.join("U7PG2-6.8.2.bin").as_path()));
        assert_eq!(std::fs::read(dir.join("U7PG2-6.8.2.bin")).unwrap(), b"abc");
        assert!(!dir.join("fallback.bin").exists());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn no_suggestion_uses_the_fallback_name() {
        let dir = temp_dir();
        let mut sink = FileSink::in_directory(dir.clone(), "fallback.bin".into());
        sink.write_chunk(b"abc").unwrap();
        sink.finish().unwrap();
        assert_eq!(sink.path(), Some(dir.join("fallback.bin").as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsafe_suggestion_is_ignored() {
        for name in ["", ".", "..", "../x.bin", "a/b.bin", "a\\b.bin", "a\nb.bin"] {
            let dir = temp_dir();
            let mut sink = FileSink::in_directory(dir.clone(), "fallback.bin".into());
            sink.suggest_file_name(name);
            sink.write_chunk(b"x").unwrap();
            // Separators differ by platform; `\` is only one on Windows.
            let used = sink.path().unwrap().to_path_buf();
            assert_eq!(used.parent().unwrap(), dir, "{name:?} escaped to {used:?}");
            if name != "a\\b.bin" {
                assert_eq!(used, dir.join("fallback.bin"), "{name:?}");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_suggestion_does_not_rename_a_file_path_target() {
        let path = temp_dir().with_extension("bin");
        let mut sink = FileSink::create(&path).unwrap();
        sink.suggest_file_name("other.bin");
        sink.write_chunk(b"abc").unwrap();
        assert_eq!(sink.path(), Some(path.as_path()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_download_into_a_directory_still_creates_the_file() {
        let dir = temp_dir();
        let mut sink = FileSink::in_directory(dir.clone(), "empty.bin".into());
        sink.finish().unwrap();
        assert_eq!(std::fs::read(dir.join("empty.bin")).unwrap(), b"");
        let _ = std::fs::remove_dir_all(&dir);
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
