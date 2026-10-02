//! Which product line each UniFi model code belongs to, so
//! `catalog --device-family USW` can select every switch instead of one model
//! at a time. See the README's "Product lines" section.
//!
//! The firmware API has no product-line field, and the model codes don't
//! encode one reliably: `USWDA23` is a UPS and not a switch, `UDMA69B` is the
//! Express 7 and not a Dream Machine, and `UDBA69F` is a bridge. So this is a
//! hand-maintained table, built from Ubiquiti's own published device list
//! (`https://static.ui.com/fingerprint/ui/public.json`, version
//! `ead6c1635a0f4f78a729a4b72e58c8c24c628063`, October 2026) and not from the
//! codes themselves.
//!
//! **How the table was built.** Each model code was looked up in that file by
//! its `shortnames`, or, for the newer `UAPA6xx`/`USWEDxx` style codes, by the
//! code's trailing hex `sysid`. Its `deviceType` and product name then decide
//! the line:
//!
//! | Line | Rule |
//! |---|---|
//! | `USW` | a switch whose product name starts with "Switch" |
//! | `U6`, `U7`, `E7` | an access point whose name starts with "Access Point U6", "U7" or "E7" |
//! | `UAP` | every other access point (the AC and nanoHD generations and older) |
//! | `USG` | a gateway named "Security Gateway ..." |
//! | `UXG` | a console named "Gateway ..." |
//! | `UDM` | the Dream Machine and Dream Machine Pro |
//! | `UX` | a console named "Express ..." |
//! | `UCK` | a Cloud Key |
//!
//! Anything else (power, bridges, LTE, travel routers, and codes the file
//! doesn't know) stays unmapped and keeps its model code as its
//! `device_family`, as before. That's deliberate: a wrong guess would silently
//! put a device in the wrong line, and not grouping it loses nothing.
//!
//! As a cross-check, models that share one firmware image (the API's `models`
//! field) must all be in one line. None of the nine such groups spans two.
//!
//! Changing a model's line changes its stored identity key, so it needs a
//! `dig --vendor unifi --redig`. The ignored live test
//! `live_every_model_code_is_mapped_or_known_unmapped` reports codes Ubiquiti
//! has added since this was written.

/// Model code to product line, sorted by code so it can be binary searched.
const LINES: &[(&str, &str)] = &[
    ("BZ2", "UAP"),
    ("BZ2LR", "UAP"),
    ("S216150", "USW"),
    ("S224250", "USW"),
    ("S224500", "USW"),
    ("S248500", "USW"),
    ("S248750", "USW"),
    ("S28150", "USW"),
    ("U2HSR", "UAP"),
    ("U2IW", "UAP"),
    ("U2Lv2", "UAP"),
    ("U2O", "UAP"),
    ("U2Sv2", "UAP"),
    ("U5O", "UAP"),
    ("U6ENT", "U6"),
    ("U6ENTIW", "U6"),
    ("U6EXT", "U6"),
    ("U6IW", "U6"),
    ("U6M", "U6"),
    ("U6MP", "U6"),
    ("U7EDU", "UAP"),
    ("U7HD", "UAP"),
    ("U7IW", "UAP"),
    ("U7IWP", "UAP"),
    ("U7LR", "UAP"),
    ("U7LT", "UAP"),
    ("U7MP", "UAP"),
    ("U7MSH", "UAP"),
    ("U7NHD", "UAP"),
    ("U7P", "UAP"),
    ("U7PG2", "UAP"),
    ("U7PIW", "U7"),
    ("U7PRO", "U7"),
    ("U7PROMAX", "U7"),
    ("U7SHD", "UAP"),
    ("UAE6", "U6"),
    ("UAIW6", "U6"),
    ("UAL6", "U6"),
    ("UALR6", "U6"),
    ("UALR6v2", "U6"),
    ("UALRPL6", "U6"),
    ("UAM6", "U6"),
    ("UAP6MP", "U6"),
    ("UAPA693", "U7"),
    ("UAPA697", "E7"),
    ("UAPA698", "E7"),
    ("UAPA699", "E7"),
    ("UAPA69E", "U7"),
    ("UAPA6A4", "U7"),
    ("UAPA6A5", "U7"),
    ("UAPA6A6", "U7"),
    ("UAPA6A9", "U7"),
    ("UAPA6AB", "E7"),
    ("UAPA6AC", "U7"),
    ("UAPA6AE", "U7"),
    ("UAPA6AF", "E7"),
    ("UAPA6B0", "U7"),
    ("UAPA6B1", "E7"),
    ("UAPA6B3", "U7"),
    ("UAPA6BA", "U7"),
    ("UAPA6BC", "E7"),
    ("UAPL6", "U6"),
    ("UCK", "UCK"),
    ("UCKG2", "UCK"),
    ("UCKP", "UCK"),
    ("UCXG", "UAP"),
    ("UDM", "UDM"),
    ("UDMA69B", "UX"),
    ("UDMB", "UAP"),
    ("UDMPRO", "UDM"),
    ("UFLHD", "UAP"),
    ("UGW3", "USG"),
    ("UGW4", "USG"),
    ("UGWXG", "USG"),
    ("UHDIW", "UAP"),
    ("UKPW", "U7"),
    ("US16P150", "USW"),
    ("US24", "USW"),
    ("US24P250", "USW"),
    ("US24P500", "USW"),
    ("US24PL2", "USW"),
    ("US24PRO", "USW"),
    ("US24PRO2", "USW"),
    ("US48", "USW"),
    ("US48P500", "USW"),
    ("US48P750", "USW"),
    ("US48PL2", "USW"),
    ("US48PRO", "USW"),
    ("US48PRO2", "USW"),
    ("US624P", "USW"),
    ("US648P", "USW"),
    ("US68P", "USW"),
    ("US6XG150", "USW"),
    ("US8", "USW"),
    ("US8P150", "USW"),
    ("US8P60", "USW"),
    ("USAGGPRO", "USW"),
    ("USC8", "USW"),
    ("USC8P450", "USW"),
    ("USF5P", "USW"),
    ("USFXG", "USW"),
    ("USL16LP", "USW"),
    ("USL16LPB", "USW"),
    ("USL16P", "USW"),
    ("USL16PB", "USW"),
    ("USL24", "USW"),
    ("USL24B", "USW"),
    ("USL24P", "USW"),
    ("USL24PB", "USW"),
    ("USL48", "USW"),
    ("USL48B", "USW"),
    ("USL48P", "USW"),
    ("USL48PB", "USW"),
    ("USL8A", "USW"),
    ("USL8LP", "USW"),
    ("USL8LPB", "USW"),
    ("USL8MP", "USW"),
    ("USLP8P", "USW"),
    ("USM8P", "USW"),
    ("USM8P210", "USW"),
    ("USM8P60", "USW"),
    ("USMINI", "USW"),
    ("USMINI2", "USW"),
    ("USPM16", "USW"),
    ("USPM16P", "USW"),
    ("USPM24", "USW"),
    ("USPM24P", "USW"),
    ("USPM48", "USW"),
    ("USPM48P", "USW"),
    ("USWED35", "USW"),
    ("USWED36", "USW"),
    ("USWED37", "USW"),
    ("USWED42", "USW"),
    ("USWED43", "USW"),
    ("USWED44", "USW"),
    ("USWED45", "USW"),
    ("USWED72", "USW"),
    ("USWED73", "USW"),
    ("USWED74", "USW"),
    ("USWED75", "USW"),
    ("USWED76", "USW"),
    ("USWED77", "USW"),
    ("USWF001", "USW"),
    ("USWF002", "USW"),
    ("USWF003", "USW"),
    ("USWF004", "USW"),
    ("USWF005", "USW"),
    ("USWF006", "USW"),
    ("USWF007", "USW"),
    ("USWF066", "USW"),
    ("USWF067", "USW"),
    ("USWF069", "USW"),
    ("USWF07D", "USW"),
    ("USXG", "USW"),
    ("USXG24", "USW"),
    ("UX", "UX"),
    ("UXBSDM", "UAP"),
    ("UXG", "UXG"),
    ("UXGA6AA", "UXG"),
    ("UXGB", "UXG"),
    ("UXGENT", "UXG"),
    ("UXGPRO", "UXG"),
    ("UXSDM", "UAP"),
];

/// Model codes seen in the live API that are deliberately not in [`LINES`].
/// Kept so the live test can tell a known gap from a newly added model.
#[cfg(test)]
const UNMAPPED: &[&str] = &[
    "U7UKU",
    "UACCEA03",
    "UACCMPOEAF",
    "UAPEA07",
    "UAVAA06",
    "UBB",
    "UBBXG",
    "UCI",
    "UDB",
    "UDBA69F",
    "UDBE802",
    "ULTE",
    "ULTEPEU",
    "ULTEPUS",
    "UMBBE630",
    "UMBBE631",
    "UMBBE633",
    "UMBBE634",
    "UP1",
    "UP6",
    "USMULT",
    "USPDA2B",
    "USPDA2C",
    "USPPDUHD",
    "USPPDUP",
    "USPRPS",
    "USPRPSP",
    "USWDA23",
    "USWDA24",
    "USWDA25",
    "USWDA26",
    "UTREA06",
    "UTREA08",
    "UXGPROV2",
];

/// The product line for a model code (the API's `platform`), if it has one.
pub fn product_line(model: &str) -> Option<&'static str> {
    LINES
        .binary_search_by_key(&model, |(code, _)| code)
        .ok()
        .map(|i| LINES[i].1)
}

/// The `device_family` for a model: its product line, or the model code
/// itself when it has none.
pub fn device_family(model: &str) -> &str {
    product_line(model).unwrap_or(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_map_to_their_line() {
        assert_eq!(product_line("U7PG2"), Some("UAP"));
        assert_eq!(product_line("USMINI"), Some("USW"));
        assert_eq!(product_line("U6ENT"), Some("U6"));
        assert_eq!(product_line("UAPA6A4"), Some("U7"));
        assert_eq!(product_line("UAPA697"), Some("E7"));
        assert_eq!(product_line("UGW4"), Some("USG"));
        assert_eq!(product_line("UXGPRO"), Some("UXG"));
        assert_eq!(product_line("UDMPRO"), Some("UDM"));
        assert_eq!(product_line("UCKG2"), Some("UCK"));
    }

    #[test]
    fn a_code_does_not_say_what_the_device_is() {
        // The prefix says switch, but it is a UPS.
        assert_eq!(product_line("USWDA23"), None);
        // The prefix says Dream Machine, but it is the Express 7.
        assert_eq!(product_line("UDMA69B"), Some("UX"));
        // The prefix says Dream Machine, but it shares firmware with access points.
        assert_eq!(product_line("UDMB"), Some("UAP"));
        // A bridge that sits in a switch-shaped code.
        assert_eq!(product_line("UDBA69F"), None);
    }

    #[test]
    fn an_unmapped_model_keeps_its_own_code_as_its_family() {
        assert_eq!(product_line("USPRPS"), None);
        assert_eq!(device_family("USPRPS"), "USPRPS");
        assert_eq!(device_family("U7PG2"), "UAP");
        assert_eq!(device_family("NEWMODEL"), "NEWMODEL");
    }

    #[test]
    fn the_table_is_sorted_with_no_duplicates_so_lookups_find_every_entry() {
        assert!(LINES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(UNMAPPED.windows(2).all(|w| w[0] < w[1]));
        for (code, _) in LINES {
            assert!(product_line(code).is_some(), "{code}");
        }
    }

    #[test]
    fn a_model_is_either_mapped_or_known_unmapped_never_both() {
        for code in UNMAPPED {
            assert_eq!(product_line(code), None, "{code} is in both lists");
        }
    }

    #[test]
    fn only_the_documented_lines_are_used() {
        const DOCUMENTED: &[&str] = &[
            "USW", "UAP", "U6", "U7", "E7", "USG", "UXG", "UDM", "UX", "UCK",
        ];
        for (code, line) in LINES {
            assert!(
                DOCUMENTED.contains(line),
                "{code} maps to undocumented line {line}"
            );
        }
    }

    /// Every model code Ubiquiti's live API lists is in [`LINES`] or
    /// [`UNMAPPED`]. A failure lists the codes added since the table was
    /// written, which need classifying.
    #[tokio::test]
    #[ignore = "hits the live Ubiquiti API"]
    async fn live_every_model_code_is_mapped_or_known_unmapped() {
        let body = reqwest::Client::new()
            .get(crate::api::list_url(&crate::api::api_base(), None))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let mut unknown: Vec<String> =
            crate::api::select_records(crate::api::parse_list(&body).unwrap().records)
                .into_iter()
                .map(|r| r.platform)
                .filter(|p| product_line(p).is_none() && !UNMAPPED.contains(&p.as_str()))
                .collect();
        unknown.sort_unstable();
        unknown.dedup();
        assert!(
            unknown.is_empty(),
            "model codes with no line and not in UNMAPPED: {unknown:?}"
        );
    }
}
