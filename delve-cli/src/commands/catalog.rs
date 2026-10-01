//! `delve catalog` — see the README's "CLI commands" section.

use std::io::Write;

use delve_core::store::MetadataStore;

use crate::cli::SelectorArgs;

pub async fn run(
    store: &dyn MetadataStore,
    selector: SelectorArgs,
    long: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let entries = store.resolve_many(&selector.into_store_selector()).await?;

    if entries.is_empty() {
        writeln!(out, "No matching firmware entries.")?;
        return Ok(());
    }

    if long {
        for s in &entries {
            let e = &s.metadata;
            writeln!(out, "id:             {}", s.id)?;
            writeln!(out, "vendor:         {}", e.vendor)?;
            writeln!(out, "device_family:  {}", e.device_family)?;
            writeln!(out, "source_url:     {}", e.source_url)?;
            writeln!(out, "version:        {}", e.version.raw)?;
            writeln!(out, "hardware:       {}", e.hardware_targets.join(", "))?;
            writeln!(out, "release_date:   {:?}", e.release_date)?;
            writeln!(
                out,
                "sha256:         {}",
                e.sha256
                    .map(|h| hex_string(&h))
                    .unwrap_or_else(|| "-".into())
            )?;
            writeln!(
                out,
                "release_notes:  {}",
                e.release_notes_url
                    .as_ref()
                    .map(|u| u.as_str())
                    .unwrap_or("-")
            )?;
            writeln!(
                out,
                "signature:      {}",
                e.signature
                    .as_ref()
                    .map(|s| format!("{} (verified: {})", s.scheme, s.verified))
                    .unwrap_or_else(|| "-".into())
            )?;
            writeln!(out, "---")?;
        }
    } else {
        // Default view: the most useful fields only, one line per entry.
        writeln!(
            out,
            "{:<36} {:<12} {:<14} {:<24} {:<16} {:<12} SHA256 (short)",
            "ID", "VENDOR", "DEVICE_FAMILY", "VERSION", "HARDWARE", "RELEASED"
        )?;
        for s in &entries {
            let e = &s.metadata;
            let short_hash = e
                .sha256
                .map(|h| hex_string(&h)[..12].to_string())
                .unwrap_or_else(|| "-".into());
            writeln!(
                out,
                "{:<36} {:<12} {:<14} {:<24} {:<16} {:<12} {}",
                s.id,
                e.vendor,
                e.device_family,
                e.version.raw,
                e.hardware_targets.join("+"),
                e.release_date
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "-".into()),
                short_hash,
            )?;
        }
    }

    Ok(())
}

fn hex_string(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::SelectorArgs;
    use crate::commands::test_support::*;

    async fn catalog(
        store: &dyn MetadataStore,
        selector: SelectorArgs,
        long: bool,
    ) -> anyhow::Result<String> {
        let mut out = Vec::new();
        run(store, selector, long, &mut out).await?;
        Ok(String::from_utf8(out).unwrap())
    }

    /// Two vendors; acme's widget line has versions whose numeric order
    /// differs from their string order, and two hardware revisions.
    async fn seeded_store() -> delve_store_sqlite::SqliteStore {
        let store = memory_store().await;
        let mut rev_b = release("widget", "1.2", &[1, 2], 4);
        rev_b.hardware = &["rev-b"];
        seed(
            &store,
            MockPlugin::new(
                "acme",
                vec![
                    release("widget", "1.2", &[1, 2], 1),
                    release("widget", "1.10", &[1, 10], 2),
                    release("gadget", "3.0", &[3, 0], 3),
                    rev_b,
                ],
            ),
        )
        .await;
        seed(
            &store,
            MockPlugin::new("globex", vec![release("sprocket", "1.0", &[1, 0], 5)]),
        )
        .await;
        store
    }

    /// Data rows of the default table view, header excluded.
    fn rows(out: &str) -> Vec<&str> {
        out.lines().skip(1).collect()
    }

    #[tokio::test]
    async fn empty_store_says_so() {
        let store = memory_store().await;
        let out = catalog(&store, selector(), false).await.unwrap();
        assert_eq!(out, "No matching firmware entries.\n");
    }

    #[tokio::test]
    async fn default_view_lists_every_entry_with_a_short_hash() {
        let store = seeded_store().await;
        let out = catalog(&store, selector(), false).await.unwrap();

        assert!(out.starts_with("ID "), "{out}");
        assert_eq!(rows(&out).len(), 5, "{out}");
        let sprocket = rows(&out)
            .into_iter()
            .find(|r| r.contains("sprocket"))
            .unwrap();
        assert!(sprocket[37..].starts_with("globex"), "{sprocket}");
        assert!(sprocket.contains("2026-09-01"), "{sprocket}");
        // sha byte 5 → "0505...", truncated to 12 hex digits.
        assert!(sprocket.ends_with(" 050505050505"), "{sprocket}");
    }

    #[tokio::test]
    async fn selector_flags_narrow_the_listing() {
        let store = seeded_store().await;

        let mut s = selector();
        s.vendor = Some("globex".into());
        assert_eq!(rows(&catalog(&store, s, false).await.unwrap()).len(), 1);

        let mut s = selector();
        s.vendor = Some("acme".into());
        s.device_family = Some("widget".into());
        assert_eq!(rows(&catalog(&store, s, false).await.unwrap()).len(), 3);

        let mut s = selector();
        s.device_family = Some("widget".into());
        s.hardware = vec!["rev-b".into()];
        let out = catalog(&store, s, false).await.unwrap();
        assert_eq!(rows(&out).len(), 1);
        assert!(rows(&out)[0].contains("rev-b"), "{out}");

        let mut s = selector();
        s.version = Some("1.10".into());
        let out = catalog(&store, s, false).await.unwrap();
        assert_eq!(rows(&out).len(), 1);
        assert!(rows(&out)[0].contains(" 1.10 "), "{out}");

        let mut s = selector();
        s.vendor = Some("initech".into());
        assert_eq!(
            catalog(&store, s, false).await.unwrap(),
            "No matching firmware entries.\n"
        );
    }

    #[tokio::test]
    async fn latest_picks_the_highest_version_per_line_numerically() {
        let store = seeded_store().await;
        let mut s = selector();
        s.device_family = Some("widget".into());
        s.hardware = vec!["rev-a".into()];
        s.latest = true;

        let out = catalog(&store, s, false).await.unwrap();
        assert_eq!(rows(&out).len(), 1, "{out}");
        // "1.10" sorts before "1.2" as a string; it must win numerically.
        assert!(rows(&out)[0].contains(" 1.10 "), "{out}");
    }

    #[tokio::test]
    async fn long_view_prints_every_field() {
        let store = seeded_store().await;
        let mut s = selector();
        s.vendor = Some("globex".into());

        let out = catalog(&store, s, true).await.unwrap();
        assert!(out.contains("vendor:         globex\n"), "{out}");
        assert!(out.contains("device_family:  sprocket\n"), "{out}");
        assert!(
            out.contains("source_url:     https://globex.example.test/sprocket/rev-a/1.0\n"),
            "{out}"
        );
        assert!(out.contains("version:        1.0\n"), "{out}");
        assert!(out.contains("hardware:       rev-a\n"), "{out}");
        assert!(
            out.contains(&format!("sha256:         {}\n", "05".repeat(32))),
            "{out}"
        );
        assert!(out.contains("release_notes:  -\n"), "{out}");
        assert!(out.contains("signature:      -\n"), "{out}");
        assert!(out.ends_with("---\n"), "{out}");
    }

    #[tokio::test]
    async fn long_view_prints_the_entry_id() {
        let store = seeded_store().await;
        let mut s = selector();
        s.vendor = Some("globex".into());

        let out = catalog(&store, s, true).await.unwrap();
        let id = out.lines().next().unwrap().strip_prefix("id:").unwrap();
        uuid::Uuid::parse_str(id.trim()).expect("id line should hold a uuid");
    }

    #[tokio::test]
    async fn an_id_printed_by_catalog_addresses_exactly_that_entry() {
        let store = seeded_store().await;
        let out = catalog(&store, selector(), false).await.unwrap();
        let sprocket = rows(&out)
            .into_iter()
            .find(|r| r.contains("sprocket"))
            .unwrap();
        let id: uuid::Uuid = sprocket.split_whitespace().next().unwrap().parse().unwrap();

        let mut s = selector();
        s.id = Some(id);
        let out = catalog(&store, s, false).await.unwrap();
        assert_eq!(rows(&out).len(), 1, "{out}");
        assert!(rows(&out)[0].contains("sprocket"), "{out}");
    }
}
