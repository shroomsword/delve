//! `delve provenance` — see the README's "CLI commands" section.

use std::io::Write;

use delve_core::store::{FirmwareKey, MetadataStore};

use crate::cli::SelectorArgs;
use crate::commands::table::write_table;

pub async fn run(
    store: &dyn MetadataStore,
    selector: SelectorArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    // provenance requires enough specificity to resolve to exactly one
    // entry (see the README's "CLI commands" section) — resolve_one enforces
    // that, returning StoreError::Ambiguous
    // if the selector was too broad.
    let entry = store
        .resolve_one(&selector.into_store_selector())
        .await?
        .ok_or_else(|| anyhow::anyhow!("no matching firmware entry"))?
        .metadata;

    // FirmwareMetadata now carries vendor/device_family itself (see its doc
    // comment in model.rs), so --id alone is enough to build the key —
    // no separate --vendor/--device-family required.
    let key = FirmwareKey::from_metadata(&entry);
    let revisions = store.history(&key).await?;

    let rows: Vec<Vec<String>> = revisions
        .iter()
        .map(|rev| {
            let hash = rev
                .metadata
                .sha256
                .map(|h| h.iter().map(|b| format!("{:02x}", b)).collect::<String>())
                .unwrap_or_else(|| "-".into());
            vec![
                rev.observed_at.to_rfc3339(),
                rev.run_id.to_string(),
                format!(
                    "{} / {}",
                    rev.metadata.version.raw,
                    &hash[..hash.len().min(12)]
                ),
            ]
        })
        .collect();
    write_table(out, &["OBSERVED_AT", "RUN_ID", "VERSION / HASH"], &rows)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::*;
    use delve_core::prelude::*;

    async fn provenance(
        store: &dyn MetadataStore,
        selector: SelectorArgs,
    ) -> anyhow::Result<String> {
        let mut out = Vec::new();
        run(store, selector, &mut out).await?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn widget_1_0() -> SelectorArgs {
        let mut s = selector();
        s.vendor = Some("acme".into());
        s.device_family = Some("widget".into());
        s.version = Some("1.0".into());
        s
    }

    #[tokio::test]
    async fn lists_every_observation_in_order() {
        let store = memory_store().await;
        let plugin = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        let releases = plugin.releases.clone();
        let registry = PluginRegistry::from_plugins(vec![Box::new(plugin)]);
        let dig = || async {
            crate::commands::dig::dig_vendors(
                &config(""),
                &registry,
                &store,
                &EventBus::new(vec![]),
                None,
                false,
                false,
            )
            .await
            .unwrap()
        };

        dig().await;
        dig().await; // same file re-observed
        releases.lock().unwrap()[0].sha = 0xab; // rebuilt under the same version
        dig().await;

        let out = provenance(&store, widget_1_0()).await.unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("OBSERVED_AT"), "{out}");
        assert_eq!(lines.len(), 4, "header plus one line per dig: {out}");
        assert!(lines[1].ends_with("1.0 / 010101010101"), "{out}");
        assert!(lines[2].ends_with("1.0 / 010101010101"), "{out}");
        assert!(lines[3].ends_with("1.0 / abababababab"), "{out}");

        // Each observation comes from a different dig.
        let run_ids: std::collections::HashSet<&str> = lines[1..]
            .iter()
            .map(|l| l.split_whitespace().nth(1).unwrap())
            .collect();
        assert_eq!(run_ids.len(), 3, "{out}");
    }

    #[tokio::test]
    async fn columns_line_up_under_their_headers() {
        let store = memory_store().await;
        seed(
            &store,
            MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]),
        )
        .await;
        let out = provenance(&store, widget_1_0()).await.unwrap();

        // The timestamp is longer than its header, and the run id is a UUID.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        let run_id = lines[0].find("RUN_ID").unwrap();
        let version = lines[0].find("VERSION / HASH").unwrap();
        assert_eq!(lines[1].find(' ').map(|i| i < run_id), Some(true), "{out}");
        assert!(lines[1][..run_id].ends_with("  "), "{out}");
        assert_eq!(
            lines[1][run_id..].split_whitespace().next().unwrap().len(),
            36
        );
        assert!(lines[1][..version].ends_with("  "), "{out}");
        assert!(lines[1][version..].starts_with("1.0 / "), "{out}");
    }

    #[tokio::test]
    async fn id_alone_resolves_the_entry_and_ignores_other_entries() {
        let store = memory_store().await;
        seed(
            &store,
            MockPlugin::new(
                "acme",
                vec![
                    release("widget", "1.0", &[1, 0], 1),
                    release("widget", "1.1", &[1, 1], 2),
                ],
            ),
        )
        .await;

        // The id comes from the store, the same way `catalog` surfaces it.
        let id = store
            .resolve_one(&widget_1_0().into_store_selector())
            .await
            .unwrap()
            .expect("widget 1.0 is stored")
            .id;

        let mut s = selector();
        s.id = Some(id);
        let out = provenance(&store, s).await.unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "header plus one observation: {out}");
        assert!(lines[1].ends_with("1.0 / 010101010101"), "{out}");
    }

    #[tokio::test]
    async fn an_unknown_id_is_an_error() {
        let store = memory_store().await;
        seed(
            &store,
            MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]),
        )
        .await;

        let mut s = selector();
        s.id = Some(uuid::Uuid::new_v4());
        let err = provenance(&store, s).await.unwrap_err();
        assert_eq!(err.to_string(), "no matching firmware entry");
    }

    #[tokio::test]
    async fn selector_matching_several_entries_is_an_error() {
        let store = memory_store().await;
        seed(
            &store,
            MockPlugin::new(
                "acme",
                vec![
                    release("widget", "1.0", &[1, 0], 1),
                    release("widget", "1.1", &[1, 1], 2),
                ],
            ),
        )
        .await;

        let mut s = selector();
        s.vendor = Some("acme".into());
        let err = provenance(&store, s).await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<StoreError>(),
                Some(StoreError::Ambiguous(2))
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn selector_matching_nothing_is_an_error() {
        let store = memory_store().await;
        seed(
            &store,
            MockPlugin::new("acme", vec![release("widget", "1.1", &[1, 1], 1)]),
        )
        .await;

        let err = provenance(&store, widget_1_0()).await.unwrap_err();
        assert_eq!(err.to_string(), "no matching firmware entry");
    }
}
