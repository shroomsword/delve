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
//! | `UDM` | a console named "Dream Machine ..." (Pro, Pro Max, Special Edition, Beast) |
//! | `UDR` | a console named "Dream Router ..." |
//! | `UCG` | a console named "Cloud Gateway ..." |
//! | `UX` | a console named "Express ..." |
//! | `UCK` | a Cloud Key |
//! | `UNVR` | a console named "Network Video Recorder ..." |
//! | `UNAS` | a NAS named "UNAS ..." |
//!
//! Anything else (power, bridges, LTE, travel routers, the "Enterprise ..."
//! consoles, the Dream Wall, and codes the file doesn't know) stays unmapped and keeps its model code as its
//! `device_family`, as before. That's deliberate: a wrong guess would silently
//! put a device in the wrong line, and not grouping it loses nothing.
//!
//! As a cross-check, models that share one firmware image (the API's `models`
//! field) must all be in one line. None of the nine such groups spans two.
//!
//! The same device list also gives each model a product name (`NAMES`), used
//! as display text only. It is not part of the identity key.
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
    ("UCGA6AD", "UCG"),
    ("UCGF", "UCG"),
    ("UCGMAX", "UCG"),
    ("UCK", "UCK"),
    ("UCKENT", "UCK"),
    ("UCKG2", "UCK"),
    ("UCKP", "UCK"),
    ("UCXG", "UAP"),
    ("UDM", "UDM"),
    ("UDMA69B", "UX"),
    ("UDMB", "UAP"),
    ("UDMEA4C", "UDM"),
    ("UDMPRO", "UDM"),
    ("UDMPROMAX", "UDM"),
    ("UDMPROSE", "UDM"),
    ("UDR", "UDR"),
    ("UDR5G", "UDR"),
    ("UDR7", "UDR"),
    ("UDRULT", "UCG"),
    ("UFLHD", "UAP"),
    ("UGW3", "USG"),
    ("UGW4", "USG"),
    ("UGWXG", "USG"),
    ("UHDIW", "UAP"),
    ("UKPW", "U7"),
    ("UNAS2B", "UNAS"),
    ("UNAS2W", "UNAS"),
    ("UNASPRO", "UNAS"),
    ("UNASPRO4", "UNAS"),
    ("UNASPRO8", "UNAS"),
    ("UNVR", "UNVR"),
    ("UNVR4", "UNVR"),
    ("UNVRAI4", "UNVR"),
    ("UNVRAI8", "UNVR"),
    ("UNVRINS", "UNVR"),
    ("UNVRPRO", "UNVR"),
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
    ("UXMAX", "UX"),
    ("UXSDM", "UAP"),
];

/// Model code to product name, such as `USMINI` to "Switch Flex Mini", for
/// notifications and `catalog`. From the same Ubiquiti device list as
/// [`LINES`] (each code's product name there), for the 195 codes that list
/// resolves to exactly one name. The two it doesn't know, `USMULT` and
/// `UXGPROV2`, have none. Sorted by code so it can be binary searched.
const NAMES: &[(&str, &str)] = &[
    ("BZ2", "Access Point"),
    ("BZ2LR", "Access Point Long-Range"),
    ("EFGCORE", "Enterprise Firewall Core"),
    ("ENAS", "Enterprise NAS"),
    ("ENVR", "Enterprise Network Video Recorder"),
    ("ENVRCORE", "Enterprise Network Video Recorder Core"),
    ("S216150", "Switch 16 PoE 150W"),
    ("S224250", "Switch 24 PoE 250W"),
    ("S224500", "Switch 24 PoE 500W"),
    ("S248500", "Switch 48 PoE 500W"),
    ("S248750", "Switch 48 PoE 750W"),
    ("S28150", "Switch 8 PoE 150W"),
    ("U2HSR", "Access Point Outdoor+"),
    ("U2IW", "Access Point In-Wall"),
    ("U2Lv2", "Access Point Long-Range"),
    ("U2O", "Access Point Outdoor"),
    ("U2Sv2", "Access Point"),
    ("U5O", "Access Point Outdoor 5"),
    ("U6ENT", "Access Point U6 Enterprise"),
    ("U6ENTIW", "Access Point U6 Enterprise In-Wall"),
    ("U6EXT", "Access Point U6 Extender"),
    ("U6IW", "Access Point U6 In-Wall"),
    ("U6M", "Access Point U6 Mesh"),
    ("U6MP", "Access Point U6 Mesh Pro"),
    ("U7EDU", "Access Point AC EDU"),
    ("U7HD", "Access Point AC HD"),
    ("U7IW", "Access Point AC In-Wall"),
    ("U7IWP", "Access Point AC In-Wall Pro"),
    ("U7LR", "Access Point AC Long-Range"),
    ("U7LT", "Access Point AC Lite"),
    ("U7MP", "Access Point AC Mesh Pro"),
    ("U7MSH", "Access Point AC Mesh"),
    ("U7NHD", "Access Point Nano HD"),
    ("U7P", "Access Point Pro"),
    ("U7PG2", "Access Point AC Pro"),
    ("U7PIW", "Access Point U7 Pro Wall"),
    ("U7PRO", "Access Point U7 Pro"),
    ("U7PROMAX", "Access Point U7 Pro Max"),
    ("U7SHD", "Access Point AC SHD"),
    ("U7UKU", "Swiss Army Knife"),
    ("UACCEA03", "Device Bridge IoT"),
    ("UACCMPOEAF", "Device Bridge"),
    ("UAE6", "Access Point U6 Extender"),
    ("UAIW6", "Access Point U6 In-Wall"),
    ("UAL6", "Access Point U6 Lite"),
    ("UALR6", "Access Point U6 Long-Range"),
    ("UALR6v2", "Access Point U6 Long-Range"),
    ("UALRPL6", "Access Point U6 Long-Range+"),
    ("UAM6", "Access Point U6 Mesh"),
    ("UAP6MP", "Access Point U6 Pro"),
    ("UAPA693", "Access Point U7 Lite"),
    ("UAPA697", "Access Point E7"),
    ("UAPA698", "Access Point E7 Campus"),
    ("UAPA699", "Access Point E7 Audience"),
    ("UAPA69E", "Access Point U7 Mesh"),
    ("UAPA6A4", "Access Point U7 Pro XGS"),
    ("UAPA6A5", "Access Point U7 In Wall"),
    ("UAPA6A6", "Access Point U7 Pro Outdoor"),
    ("UAPA6A9", "Access Point U7 Pro XG"),
    ("UAPA6AB", "Access Point E7 Audience"),
    ("UAPA6AC", "Access Point U7 Pro XGS"),
    ("UAPA6AE", "Access Point U7 Pro XG"),
    ("UAPA6AF", "Access Point E7 Audience Indoor"),
    ("UAPA6B0", "Access Point U7 Pro Outdoor"),
    ("UAPA6B1", "Access Point E7 Campus"),
    ("UAPA6B3", "Access Point U7 Long-Range"),
    ("UAPA6BA", "Access Point U7 Pro XG Wall"),
    ("UAPA6BC", "Access Point E7 Campus Indoor"),
    ("UAPEA07", "AirWire"),
    ("UAPL6", "Access Point U6+"),
    ("UAVAA06", "EAV Bridge"),
    ("UBB", "Building Bridge"),
    ("UBBXG", "Building Bridge XG"),
    ("UCGA6AD", "Cloud Gateway Industrial"),
    ("UCGF", "Cloud Gateway Fiber"),
    ("UCGMAX", "Cloud Gateway Max"),
    ("UCI", "Cable Internet"),
    ("UCK", "CloudKey"),
    ("UCKENT", "CloudKey Enterprise"),
    ("UCKG2", "CloudKey"),
    ("UCKP", "CloudKey+"),
    ("UCXG", "Access Point XG"),
    ("UDB", "Device Bridge Pro"),
    ("UDBA69F", "Device Bridge Switch"),
    ("UDBE802", "Device Bridge Pro Sector"),
    ("UDM", "Dream Machine"),
    ("UDMA69B", "Express 7"),
    ("UDMB", "Access Point BeaconHD"),
    ("UDMEA4C", "Dream Machine Beast"),
    ("UDMENT", "Enterprise Firewall"),
    ("UDMPRO", "Dream Machine Pro"),
    ("UDMPROMAX", "Dream Machine Pro Max"),
    ("UDMPROSE", "Dream Machine Special Edition"),
    ("UDR", "Dream Router"),
    ("UDR5G", "Dream Router 5G Max"),
    ("UDR7", "Dream Router 7"),
    ("UDRULT", "Cloud Gateway Ultra"),
    ("UDW", "Dream Wall"),
    ("UFLHD", "Access Point FlexHD"),
    ("UGW3", "Security Gateway 3P"),
    ("UGW4", "Security Gateway Pro"),
    ("UGWXG", "Security Gateway XG 8"),
    ("UHDIW", "Access Point In-Wall HD"),
    ("UKPW", "Access Point U7 Outdoor"),
    ("ULTE", "LTE Backup"),
    ("ULTEPEU", "LTE Backup Pro"),
    ("ULTEPUS", "LTE Backup Pro"),
    ("UMBBE630", "U5G Max"),
    ("UMBBE631", "U5G Max Outdoor"),
    ("UMBBE633", "U5G Backup"),
    ("UMBBE634", "U5G Backup"),
    ("UNAS2B", "UNAS 2"),
    ("UNAS2W", "UNAS 2"),
    ("UNASPRO", "UNAS Pro"),
    ("UNASPRO4", "UNAS Pro 4"),
    ("UNASPRO8", "UNAS Pro 8"),
    ("UNVR", "Network Video Recorder"),
    ("UNVR4", "Network Video Recorder"),
    ("UNVRAI4", "Network Video Recorder Gen 2"),
    ("UNVRAI8", "Network Video Recorder Gen 2 Pro"),
    ("UNVRINS", "Network Video Recorder Instant"),
    ("UNVRPRO", "Network Video Recorder Pro"),
    ("UP1", "SmartPower Plug"),
    ("UP6", "SmartPower Strip"),
    ("US16P150", "Switch 16 PoE 150W"),
    ("US24", "Switch 24"),
    ("US24P250", "Switch 24 PoE 250W"),
    ("US24P500", "Switch 24 PoE 500W"),
    ("US24PL2", "Switch L2 24 PoE"),
    ("US24PRO", "Switch Pro 24 PoE"),
    ("US24PRO2", "Switch Pro 24"),
    ("US48", "Switch 48"),
    ("US48P500", "Switch 48 PoE 500W"),
    ("US48P750", "Switch 48 PoE 750W"),
    ("US48PL2", "Switch L2 48 PoE"),
    ("US48PRO", "Switch Pro 48 PoE"),
    ("US48PRO2", "Switch Pro 48"),
    ("US624P", "Switch Enterprise 24 PoE"),
    ("US648P", "Switch Enterprise 48 PoE"),
    ("US68P", "Switch Enterprise 8 PoE"),
    ("US6XG150", "Switch XG 6 PoE"),
    ("US8", "Switch 8"),
    ("US8P150", "Switch 8 PoE 150W"),
    ("US8P60", "Switch 8 60W"),
    ("USAGGPRO", "Switch Pro Aggregation"),
    ("USC8", "Switch 8"),
    ("USC8P450", "Switch Industrial"),
    ("USF5P", "Switch Flex"),
    ("USFXG", "Switch Flex XG"),
    ("USL16LP", "Switch Lite 16 PoE"),
    ("USL16LPB", "Switch Lite 16 PoE"),
    ("USL16P", "Switch 16 PoE"),
    ("USL16PB", "Switch 16 PoE"),
    ("USL24", "Switch 24"),
    ("USL24B", "Switch 24"),
    ("USL24P", "Switch 24 PoE"),
    ("USL24PB", "Switch 24 PoE"),
    ("USL48", "Switch 48"),
    ("USL48B", "Switch 48"),
    ("USL48P", "Switch 48 PoE"),
    ("USL48PB", "Switch 48 PoE"),
    ("USL8A", "Switch Aggregation"),
    ("USL8LP", "Switch Lite 8 PoE"),
    ("USL8LPB", "Switch Lite 8 PoE"),
    ("USL8MP", "Switch Mission Critical"),
    ("USLP8P", "Switch Pro 8 PoE"),
    ("USM8P", "Switch Ultra"),
    ("USM8P210", "Switch Ultra 210W"),
    ("USM8P60", "Switch Ultra 60W"),
    ("USMINI", "Switch Flex Mini"),
    ("USMINI2", "Switch Flex Mini"),
    ("USPDA2B", "UPS 2U Pro"),
    ("USPDA2C", "UPS 2U Pro"),
    ("USPM16", "Switch Pro Max 16"),
    ("USPM16P", "Switch Pro Max 16 PoE"),
    ("USPM24", "Switch Pro Max 24"),
    ("USPM24P", "Switch Pro Max 24 PoE"),
    ("USPM48", "Switch Pro Max 48"),
    ("USPM48P", "Switch Pro Max 48 PoE"),
    ("USPPDUHD", "Power Distribution Hi-Density"),
    ("USPPDUP", "Power Distribution Pro"),
    ("USPRPS", "Power Backup"),
    ("USPRPSP", "Power Backup Pro"),
    ("USWDA23", "UPS Tower"),
    ("USWDA24", "UPS Tower"),
    ("USWDA25", "UPS 2U"),
    ("USWDA26", "UPS 2U"),
    ("USWED35", "Switch Flex 2.5G 5"),
    ("USWED36", "Switch Flex 2.5G 8"),
    ("USWED37", "Switch Flex 2.5G 8 PoE"),
    ("USWED42", "Switch Pro XG 48 PoE"),
    ("USWED43", "Switch Pro XG 48"),
    ("USWED44", "Switch Pro XG 24 PoE"),
    ("USWED45", "Switch Pro XG 24"),
    ("USWED72", "Switch Pro HD 24 PoE"),
    ("USWED73", "Switch Pro HD 24"),
    ("USWED74", "Switch WAN Relay SFP+"),
    ("USWED75", "Switch WAN Relay RJ45"),
    ("USWED76", "Switch Pro XG 8 PoE"),
    ("USWED77", "Switch Pro XG 10 PoE"),
    ("USWF001", "Switch Enterprise AV XG 24 PoE"),
    ("USWF002", "Switch Enterprise AV Fiber"),
    ("USWF003", "Switch Pro XG Aggregation"),
    ("USWF004", "Switch Enterprise Campus 24S PoE"),
    ("USWF005", "Switch Enterprise Campus 24S"),
    ("USWF006", "Switch Enterprise Campus 48S PoE"),
    ("USWF007", "Switch Enterprise Campus 48S"),
    ("USWF066", "Switch Enterprise Campus Aggregation"),
    ("USWF067", "Switch Enterprise Campus 24 PoE"),
    ("USWF069", "Switch Enterprise Campus 48 PoE"),
    ("USWF07D", "Switch Enterprise Campus Core"),
    ("USXG", "Switch XG 16"),
    ("USXG24", "Switch EnterpriseXG 24"),
    ("UTREA06", "Travel Router"),
    ("UTREA08", "Travel Router Long-Range"),
    ("UX", "Express"),
    ("UXBSDM", "WiFi BaseStation XG"),
    ("UXG", "Gateway Lite"),
    ("UXGA6AA", "Gateway Fiber"),
    ("UXGB", "Gateway Max"),
    ("UXGENT", "Gateway Enterprise"),
    ("UXGPRO", "Gateway Pro"),
    ("UXMAX", "Express 7"),
    ("UXSDM", "WiFi BaseStation XG"),
];

/// The product name for a model code (the API's `platform`), if known.
pub fn product_name(model: &str) -> Option<&'static str> {
    NAMES
        .binary_search_by_key(&model, |(code, _)| code)
        .ok()
        .map(|i| NAMES[i].1)
}

/// Model codes seen in the live API that are deliberately not in [`LINES`].
/// Kept so the live test can tell a known gap from a newly added model.
#[cfg(test)]
const UNMAPPED: &[&str] = &[
    "EFGCORE",
    "ENAS",
    "ENVR",
    "ENVRCORE",
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
    "UDMENT",
    "UDW",
    "ULTE",
    "ULTEPEU",
    "ULTEPUS",
    "UMBBE630",
    "UMBBE631",
    "UMBBE633",
    "UMBBE634",
    "UNAS2",
    "UNAS4",
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
        assert_eq!(product_line("UDMPROMAX"), Some("UDM"));
        assert_eq!(product_line("UDR7"), Some("UDR"));
        assert_eq!(product_line("UCGMAX"), Some("UCG"));
        assert_eq!(product_line("UNVRPRO"), Some("UNVR"));
        assert_eq!(product_line("UNASPRO8"), Some("UNAS"));
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
    fn product_names_come_from_the_table() {
        assert_eq!(product_name("USMINI"), Some("Switch Flex Mini"));
        assert_eq!(product_name("U7PG2"), Some("Access Point AC Pro"));
        assert_eq!(product_name("USPRPS"), Some("Power Backup"));
        assert_eq!(product_name("NEWMODEL"), None);
    }

    #[test]
    fn the_name_table_is_sorted_and_names_every_mapped_model() {
        assert!(NAMES.windows(2).all(|w| w[0].0 < w[1].0));
        for (code, _) in LINES {
            assert!(
                product_name(code).is_some(),
                "{code} is in a line but has no name"
            );
        }
    }

    #[test]
    fn the_known_unmapped_models_are_named_except_those_ubiquiti_does_not_know() {
        for code in UNMAPPED {
            if !["USMULT", "UXGPROV2", "UNAS2", "UNAS4"].contains(code) {
                assert!(product_name(code).is_some(), "{code} has no name");
            }
        }
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
            "USW", "UAP", "U6", "U7", "E7", "USG", "UXG", "UDM", "UDR", "UCG", "UX", "UCK", "UNVR",
            "UNAS",
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
        let mut records = Vec::new();
        for product in crate::api::SUPPORTED_PRODUCTS {
            let body = reqwest::Client::new()
                .get(crate::api::list_url(
                    &crate::api::api_base(),
                    Some(product),
                    None,
                ))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            records.extend(crate::api::parse_list(&body).unwrap().records);
        }
        let mut unknown: Vec<String> =
            crate::api::select_records(records, crate::api::SUPPORTED_PRODUCTS)
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
