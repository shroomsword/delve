//! Cisco's software-download catalog: the three JSON documents the
//! software.cisco.com download pages are built from.
//!
//! - `GET /services/catalog/v1/products?mdfid=<id>` lists the children of one
//!   node in the product tree (`mdfId` is Cisco's id for a node, and
//!   `leafNodeFlag` says whether it is a product with software or a category
//!   with more nodes below it).
//! - `GET /services/catalog/v1/softwareTypes?mdfid=<id>` lists the kinds of
//!   software a product has (`IOS XE Software`, `NBAR2 Protocol Packs`, ...)
//!   with the `softwareId` of each.
//! - `GET /services/catalog/v1/releases?mdfid=<id>&softwareId=<id>` lists the
//!   releases of one kind of software of one product: version, train, life
//!   cycle, whether Cisco suggests it, and whether it is deferred.
//!
//! The shapes here come from captured responses (kept in `testdata/`): the
//! top of the tree, the software types of a Catalyst (9300), and the releases
//! of the Catalyst 9300 and of an SF220. The two differ in what they carry:
//! the 9300's versions have a train name (`Dublin-17.12.8`) and a life cycle,
//! the SF220's are plain `1.2.1.5` with neither.
//! Nothing here has been run against the live endpoints. They answered an
//! unauthenticated client with 401, so the code takes the response bodies and
//! a [`CatalogSource`] to fetch them, and leaves how a request is allowed
//! through to whoever implements the source. [`read_catalog`] ties the steps
//! together: walk the tree below a node, find each product's software type by
//! name, read that type's releases.
//!
//! What the releases document does **not** carry: a release date, a checksum,
//! a download link or release notes. The metadata mapped from it has none of
//! those, which is how `sha256: None` and `release_date: None` come about.

use std::collections::{HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use chrono::Utc;
use delve_core::model::{FirmwareMetadata, FirmwareRef, VersionKey, VersionScheme};
use delve_core::plugin::PluginError;
use serde::Deserialize;
use url::Url;

use crate::version::parse_ios_xe_version;

const BASE: &str = "https://software.cisco.com/services/catalog/v1";

/// The URL listing the children of product-tree node `mdf_id`.
pub fn products_url(mdf_id: &str) -> Url {
    let mut url = Url::parse(&format!("{BASE}/products")).expect("a constant URL");
    url.query_pairs_mut().append_pair("mdfid", mdf_id);
    url
}

/// The URL listing the releases of product `mdf_id`'s software `software_id`.
pub fn releases_url(mdf_id: &str, software_id: &str) -> Url {
    let mut url = Url::parse(&format!("{BASE}/releases")).expect("a constant URL");
    url.query_pairs_mut()
        .append_pair("mdfid", mdf_id)
        .append_pair("softwareId", software_id);
    url
}

/// The URL listing the kinds of software of product `mdf_id`.
pub fn software_types_url(mdf_id: &str) -> Url {
    let mut url = Url::parse(&format!("{BASE}/softwareTypes")).expect("a constant URL");
    url.query_pairs_mut().append_pair("mdfid", mdf_id);
    url
}

// ---- the product tree ------------------------------------------------

/// One node of the product tree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductNode {
    pub mdf_id: String,
    /// `Category` for every node in the sample (the top of the tree).
    #[serde(default)]
    pub meta_class: String,
    /// The node's name, such as `Switches`.
    #[serde(default)]
    pub mdf_concept: String,
    #[serde(default)]
    pub parent_mdf_id: String,
    /// `Y` for a node that is a product, `N` for one with nodes below it.
    #[serde(default)]
    pub leaf_node_flag: String,
}

impl ProductNode {
    pub fn is_leaf(&self) -> bool {
        self.leaf_node_flag.eq_ignore_ascii_case("Y")
    }
}

#[derive(Deserialize)]
struct ProductsResponse {
    #[serde(default)]
    response: Vec<ProductNode>,
}

/// The nodes in a `products` response body.
pub fn parse_products(body: &str) -> Result<Vec<ProductNode>, PluginError> {
    serde_json::from_str::<ProductsResponse>(body)
        .map(|r| r.response)
        .map_err(|e| PluginError::Parse(format!("Cisco catalog products: {e}")))
}

/// Where the response bodies come from. An implementation sends the request
/// and returns the body, and does its own throttling.
#[async_trait]
pub trait CatalogSource: Send + Sync {
    /// The body of [`products_url`] for `mdf_id`.
    async fn products(&self, mdf_id: &str) -> Result<String, PluginError>;
    /// The body of [`software_types_url`] for `mdf_id`.
    async fn software_types(&self, mdf_id: &str) -> Result<String, PluginError>;
    /// The body of [`releases_url`] for `mdf_id` and `software_id`.
    async fn releases(&self, mdf_id: &str, software_id: &str) -> Result<String, PluginError>;
}

/// A product found by [`find_products`], with the names of the nodes above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Product {
    pub node: ProductNode,
    /// From the node `find_products` started at (not included) down to this
    /// one: `["Switches", "LAN Switches - Access", "Catalyst 9300 Series"]`.
    pub path: Vec<String>,
}

/// How much of the tree [`find_products`] may read. The tree has thousands of
/// products, so a walk that isn't bounded is a crawl of the whole catalog.
#[derive(Debug, Clone, Copy)]
pub struct WalkLimits {
    /// Requests to make, at most. Reaching it is an error, not a short result.
    pub max_requests: usize,
    /// Levels below the starting node to descend into.
    pub max_depth: usize,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self {
            max_requests: 50,
            max_depth: 8,
        }
    }
}

/// Walks the tree below `root`, breadth first, and returns the products
/// (leaf nodes) in the order they were found.
///
/// A node seen twice is read once. Running out of `limits.max_requests`
/// before the walk is done fails with an error naming the limit rather than
/// returning the part that was read, so a partial result is never taken for
/// the whole subtree. Nodes deeper than `limits.max_depth` are not read.
pub async fn find_products(
    source: &dyn CatalogSource,
    root: &str,
    limits: WalkLimits,
) -> Result<Vec<Product>, PluginError> {
    let mut found = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([root.to_string()]);
    let mut queue: VecDeque<(String, Vec<String>)> = VecDeque::from([(root.to_string(), vec![])]);
    let mut requests = 0;

    while let Some((mdf_id, path)) = queue.pop_front() {
        if requests == limits.max_requests {
            return Err(PluginError::UnexpectedResponse(format!(
                "the product walk below {root} needs more than {} requests; \
                 start lower in the tree or raise the limit",
                limits.max_requests
            )));
        }
        requests += 1;
        let body = source.products(&mdf_id).await?;
        for node in parse_products(&body)? {
            if !seen.insert(node.mdf_id.clone()) {
                continue;
            }
            let mut node_path = path.clone();
            node_path.push(node.mdf_concept.clone());
            if node.is_leaf() {
                found.push(Product {
                    node,
                    path: node_path,
                });
            } else if node_path.len() < limits.max_depth {
                queue.push_back((node.mdf_id, node_path));
            }
        }
    }
    Ok(found)
}

// ---- software types --------------------------------------------------

/// One kind of software a product has, such as `IOS XE Software`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoftwareType {
    pub name: String,
    /// The id the `releases` endpoint takes. Cisco sends it as a number.
    pub software_id: u64,
    /// 1 or 2 in the sample; what the levels mean isn't in the response.
    #[serde(default)]
    pub access_level: Option<u8>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SoftwareTypesResponse {
    #[serde(default)]
    software_type_list: Vec<SoftwareType>,
}

/// The software types in a `softwareTypes` response body.
pub fn parse_software_types(body: &str) -> Result<Vec<SoftwareType>, PluginError> {
    serde_json::from_str::<SoftwareTypesResponse>(body)
        .map(|r| r.software_type_list)
        .map_err(|e| PluginError::Parse(format!("Cisco catalog software types: {e}")))
}

/// The software type called `name`, ignoring case and surrounding space.
pub fn find_software_type<'a>(types: &'a [SoftwareType], name: &str) -> Option<&'a SoftwareType> {
    let name = name.trim();
    types
        .iter()
        .find(|t| t.name.trim().eq_ignore_ascii_case(name))
}

// ---- releases --------------------------------------------------------

/// One entry of a `releases` list, as Cisco sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseEntry {
    release_version: String,
    /// The train, such as `Dublin-17.12`. Empty on some entries.
    #[serde(default)]
    bucket2: String,
    /// `Y` when Cisco suggests this release.
    #[serde(default)]
    is_suggested: String,
    /// `AVAILABLE` or `DEFERRED`.
    #[serde(default)]
    release_indicator: String,
    #[serde(default)]
    life_cycle: String,
}

/// The lists of a `releases` response. `allRelease` has the available
/// releases; `latestRelease` and `suggestedRelease` repeat some of them;
/// `deferredRelease` has the deferred ones, including some that `allRelease`
/// doesn't; `betaRelease` was empty in the sample.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleasesResponse {
    #[serde(default)]
    all_release: Vec<ReleaseEntry>,
    #[serde(default)]
    latest_release: Vec<ReleaseEntry>,
    #[serde(default)]
    suggested_release: Vec<ReleaseEntry>,
    #[serde(default)]
    deferred_release: Vec<ReleaseEntry>,
    #[serde(default)]
    beta_release: Vec<ReleaseEntry>,
}

/// Whether Cisco offers a release for download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// Pulled by Cisco after publishing (usually for a defect).
    Deferred,
}

/// One release of a product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// As Cisco publishes it, such as `Dublin-17.12.8` or `IOSXE-26.2.1`.
    pub version: String,
    /// `Dublin-17.12`, when Cisco gives one.
    pub train: Option<String>,
    /// `GA`, `MD` or `ED`, when Cisco gives one.
    pub life_cycle: Option<String>,
    pub suggested: bool,
    pub availability: Availability,
}

/// The releases in a `releases` response body: each version once, in the
/// order they first appear across the lists.
///
/// A version in several lists is one release: it is suggested if any entry
/// says so, and deferred if any entry says so. The sample lists
/// `IOSXE-17.15.4b` as available in `allRelease` and as deferred in
/// `deferredRelease`; deferred is taken, since offering a pulled release as
/// current is the worse mistake.
pub fn parse_releases(body: &str) -> Result<Vec<Release>, PluginError> {
    let response: ReleasesResponse = serde_json::from_str(body)
        .map_err(|e| PluginError::Parse(format!("Cisco catalog releases: {e}")))?;

    let mut releases: Vec<Release> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let lists = [
        response.all_release,
        response.beta_release,
        response.latest_release,
        response.suggested_release,
        response.deferred_release,
    ];
    for entry in lists.into_iter().flatten() {
        let version = entry.release_version.trim().to_string();
        if version.is_empty() {
            continue;
        }
        let suggested = entry.is_suggested.eq_ignore_ascii_case("Y");
        let deferred = entry.release_indicator.eq_ignore_ascii_case("DEFERRED");
        let slot = *index.entry(version.clone()).or_insert_with(|| {
            releases.push(Release {
                version,
                train: None,
                life_cycle: None,
                suggested: false,
                availability: Availability::Available,
            });
            releases.len() - 1
        });
        let release = &mut releases[slot];
        release.suggested |= suggested;
        if deferred {
            release.availability = Availability::Deferred;
        }
        if release.train.is_none() && !entry.bucket2.is_empty() {
            release.train = Some(entry.bucket2);
        }
        if release.life_cycle.is_none() && !entry.life_cycle.is_empty() {
            release.life_cycle = Some(entry.life_cycle);
        }
    }
    Ok(releases)
}

/// `Dublin-17.12.8` as (`Some("Dublin")`, `"17.12.8"`); a version with no
/// train name as (`None`, the version).
fn split_train_name(version: &str) -> (Option<&str>, &str) {
    match version.split_once('-') {
        Some((name, rest))
            if !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphabetic())
                && rest.starts_with(|c: char| c.is_ascii_digit()) =>
        {
            (Some(name), rest)
        }
        _ => (None, version),
    }
}

/// The version key for a version as Cisco publishes it. The train name
/// doesn't take part in the order: `IOSXE-17.18.4` is newer than
/// `Dublin-17.12.8` because 17.18 is newer than 17.12. A number that isn't
/// plain dotted decimal with at most one rebuild letter (`17.11.99SW`) is
/// kept as published and has no order.
pub fn version_key(version: &str) -> VersionKey {
    let (_, numbers) = split_train_name(version);
    match parse_ios_xe_version(numbers) {
        Some(ordinal) => VersionKey {
            raw: version.to_string(),
            scheme: VersionScheme::VendorNumeric,
            ordinal: Some(ordinal),
        },
        None => VersionKey::opaque(version.to_string()),
    }
}

/// The product and kind of software a `releases` response is for.
#[derive(Debug, Clone)]
pub struct Target {
    pub mdf_id: String,
    pub software_id: String,
    /// The product's name, such as `Catalyst 9300 Switch`.
    pub name: String,
}

impl Target {
    pub fn new(product: &Product, software: &SoftwareType) -> Self {
        Self {
            mdf_id: product.node.mdf_id.clone(),
            software_id: software.software_id.to_string(),
            name: product.node.mdf_concept.clone(),
        }
    }
}

/// A release as the engine's `FirmwareRef` and `FirmwareMetadata`.
///
/// `source_url` is the `releases` URL with the version as its fragment, so
/// each release has its own and the URL still fetches the list that holds it.
pub fn to_firmware(
    release: &Release,
    target: &Target,
    vendor: &str,
    device_family: &str,
) -> (FirmwareRef, FirmwareMetadata) {
    let mut source_url = releases_url(&target.mdf_id, &target.software_id);
    source_url.set_fragment(Some(&release.version));
    let firmware_ref = FirmwareRef {
        vendor: vendor.to_string(),
        device_family: device_family.to_string(),
        source_url: source_url.clone(),
        discovered_at: Utc::now(),
    };
    let metadata = FirmwareMetadata {
        vendor: vendor.to_string(),
        device_family: device_family.to_string(),
        source_url,
        version: version_key(&release.version),
        release_date: None,
        sha256: None,
        signature: None,
        hardware_targets: vec![target.name.clone()],
        release_notes_url: None,
        display_name: None,
    };
    (firmware_ref, metadata)
}

// ---- reading a subtree -----------------------------------------------

/// A product's releases of the software type that was asked for.
#[derive(Debug, Clone)]
pub struct ProductReleases {
    pub product: Product,
    pub software: SoftwareType,
    pub releases: Vec<Release>,
}

/// What [`read_catalog`] found below a node.
#[derive(Debug, Clone, Default)]
pub struct CatalogReading {
    pub read: Vec<ProductReleases>,
    /// Products with no software type of the name asked for. Most of a
    /// subtree is usually this: the other kinds of software, and products
    /// that have none of the kind.
    pub without_software: Vec<Product>,
}

/// Makes at most `left` requests through `inner`, and fails the next one.
struct Budget<'a> {
    inner: &'a dyn CatalogSource,
    left: std::sync::atomic::AtomicUsize,
    limit: usize,
    root: &'a str,
}

impl Budget<'_> {
    fn spend(&self) -> Result<(), PluginError> {
        use std::sync::atomic::Ordering::Relaxed;
        self.left
            .fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1))
            .map(|_| ())
            .map_err(|_| {
                PluginError::UnexpectedResponse(format!(
                    "reading the catalog below {} needs more than {} requests; \
                     start lower in the tree or raise the limit",
                    self.root, self.limit
                ))
            })
    }
}

#[async_trait]
impl CatalogSource for Budget<'_> {
    async fn products(&self, mdf_id: &str) -> Result<String, PluginError> {
        self.spend()?;
        self.inner.products(mdf_id).await
    }

    async fn software_types(&self, mdf_id: &str) -> Result<String, PluginError> {
        self.spend()?;
        self.inner.software_types(mdf_id).await
    }

    async fn releases(&self, mdf_id: &str, software_id: &str) -> Result<String, PluginError> {
        self.spend()?;
        self.inner.releases(mdf_id, software_id).await
    }
}

/// Reads the releases of every product below `root`: walks the tree to its
/// products, asks each for its software types, and for those that have one
/// called `software_name` (`IOS XE Software`, say) reads its releases.
///
/// `limits.max_requests` is for the whole read, the tree walk and the
/// per-product requests together, and running out is an error, not a short
/// result. A product that has no such software type is listed in
/// [`CatalogReading::without_software`], not an error.
pub async fn read_catalog(
    source: &dyn CatalogSource,
    root: &str,
    software_name: &str,
    limits: WalkLimits,
) -> Result<CatalogReading, PluginError> {
    let budget = Budget {
        inner: source,
        left: limits.max_requests.into(),
        limit: limits.max_requests,
        root,
    };
    let mut reading = CatalogReading::default();
    for product in find_products(&budget, root, limits).await? {
        let mdf_id = &product.node.mdf_id;
        let types = parse_software_types(&budget.software_types(mdf_id).await?)?;
        let Some(software) = find_software_type(&types, software_name).cloned() else {
            reading.without_software.push(product);
            continue;
        };
        let body = budget
            .releases(mdf_id, &software.software_id.to_string())
            .await?;
        let releases = parse_releases(&body)?;
        reading.read.push(ProductReleases {
            product,
            software,
            releases,
        });
    }
    Ok(reading)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRODUCTS: &str = include_str!("../testdata/catalog-products-root.json");
    const C9300: &str = include_str!("../testdata/catalog-releases-c9300.json");

    fn release<'a>(all: &'a [Release], version: &str) -> &'a Release {
        all.iter()
            .find(|r| r.version == version)
            .unwrap_or_else(|| panic!("no release {version}"))
    }

    #[test]
    fn the_urls_carry_the_ids_as_query_parameters() {
        assert_eq!(
            products_url("268437593").as_str(),
            "https://software.cisco.com/services/catalog/v1/products?mdfid=268437593"
        );
        assert_eq!(
            releases_url("286315874", "282046477").as_str(),
            "https://software.cisco.com/services/catalog/v1/releases?mdfid=286315874&softwareId=282046477"
        );
    }

    #[test]
    fn the_root_of_the_tree_is_twenty_categories_below_one_parent() {
        let nodes = parse_products(PRODUCTS).unwrap();
        assert_eq!(nodes.len(), 20);
        assert!(nodes.iter().all(|n| n.parent_mdf_id == "268437593"));
        assert!(nodes.iter().all(|n| !n.is_leaf()));
        let switches = nodes.iter().find(|n| n.mdf_concept == "Switches").unwrap();
        assert_eq!(switches.mdf_id, "268438038");
    }

    #[test]
    fn a_body_that_is_not_json_is_a_parse_error() {
        assert!(matches!(
            parse_products("<html>"),
            Err(PluginError::Parse(_))
        ));
        assert!(matches!(
            parse_releases("<html>"),
            Err(PluginError::Parse(_))
        ));
    }

    #[test]
    fn an_empty_response_has_no_nodes_and_no_releases() {
        assert!(parse_products("{}").unwrap().is_empty());
        assert!(parse_releases("{}").unwrap().is_empty());
    }

    #[test]
    fn the_c9300_has_ninety_seven_distinct_releases() {
        // 95 in allRelease, plus two that only deferredRelease lists.
        let releases = parse_releases(C9300).unwrap();
        assert_eq!(releases.len(), 97);
        let distinct: HashSet<_> = releases.iter().map(|r| &r.version).collect();
        assert_eq!(distinct.len(), 97);
    }

    #[test]
    fn a_release_only_the_deferred_list_has_is_included_and_deferred() {
        let releases = parse_releases(C9300).unwrap();
        for version in ["Cupertino-17.9.6", "Everest-16.5.1a"] {
            assert_eq!(
                release(&releases, version).availability,
                Availability::Deferred,
                "{version}"
            );
        }
    }

    #[test]
    fn a_version_listed_as_available_and_as_deferred_is_deferred() {
        let releases = parse_releases(C9300).unwrap();
        assert_eq!(
            release(&releases, "IOSXE-17.15.4b").availability,
            Availability::Deferred
        );
        assert_eq!(
            release(&releases, "IOSXE-17.15.4d").availability,
            Availability::Available
        );
    }

    #[test]
    fn exactly_the_suggested_releases_are_suggested() {
        let releases = parse_releases(C9300).unwrap();
        let mut suggested: Vec<_> = releases
            .iter()
            .filter(|r| r.suggested)
            .map(|r| r.version.as_str())
            .collect();
        suggested.sort();
        assert_eq!(suggested, ["IOSXE-17.15.6", "IOSXE-17.18.4"]);
    }

    #[test]
    fn train_and_life_cycle_come_from_whichever_list_has_them() {
        let releases = parse_releases(C9300).unwrap();
        let r = release(&releases, "Dublin-17.12.8");
        assert_eq!(r.train.as_deref(), Some("Dublin-17.12"));
        assert_eq!(r.life_cycle.as_deref(), Some("MD"));
        // The deferred-only entries carry them too.
        let r = release(&releases, "Cupertino-17.9.6");
        assert_eq!(r.train.as_deref(), Some("Cupertino-17.9"));
        // An entry with an empty train and life cycle has neither.
        let r = release(&releases, "IOSXE-17.15.4b");
        assert_eq!((r.train.as_deref(), r.life_cycle.as_deref()), (None, None));
    }

    #[test]
    fn the_train_name_is_not_part_of_the_order() {
        let newer = version_key("IOSXE-17.18.4");
        let older = version_key("Dublin-17.12.8");
        assert!(matches!(newer.scheme, VersionScheme::VendorNumeric));
        assert_eq!(newer.ordinal, Some(vec![17, 18, 4, 0]));
        assert_eq!(older.ordinal, Some(vec![17, 12, 8, 0]));
        assert!(newer.ordinal > older.ordinal);
        assert_eq!(newer.raw, "IOSXE-17.18.4", "kept as Cisco publishes it");
    }

    #[test]
    fn a_rebuild_letter_sorts_after_the_release_it_rebuilds() {
        let base = version_key("Cupertino-17.9.6");
        let rebuilt = version_key("Cupertino-17.9.6a");
        assert!(rebuilt.ordinal > base.ordinal);
    }

    #[test]
    fn a_version_that_is_not_plain_dotted_decimal_has_no_order() {
        let key = version_key("Dublin-17.11.99SW");
        assert!(matches!(key.scheme, VersionScheme::Opaque));
        assert_eq!(key.ordinal, None);
        assert_eq!(key.raw, "Dublin-17.11.99SW");
    }

    #[test]
    fn a_version_without_a_train_name_is_read_as_it_is() {
        assert_eq!(version_key("17.9.4a").ordinal, Some(vec![17, 9, 4, 1]));
        assert_eq!(version_key("1.1.4.1").ordinal, Some(vec![1, 1, 4, 1, 0]));
    }

    #[test]
    fn every_c9300_version_but_the_odd_one_out_has_an_order() {
        let releases = parse_releases(C9300).unwrap();
        let unordered: Vec<_> = releases
            .iter()
            .filter(|r| version_key(&r.version).ordinal.is_none())
            .map(|r| r.version.as_str())
            .collect();
        assert_eq!(unordered, ["Dublin-17.11.99SW"]);
    }

    fn c9300() -> Target {
        Target {
            mdf_id: "286315874".into(),
            software_id: "282046477".into(),
            name: "Catalyst 9300 Switch".into(),
        }
    }

    #[test]
    fn each_release_gets_its_own_source_url_that_names_the_list_it_is_in() {
        let releases = parse_releases(C9300).unwrap();
        let urls: HashSet<String> = releases
            .iter()
            .map(|r| to_firmware(r, &c9300(), "cisco", "catalyst-9300").0)
            .map(|f| f.source_url.to_string())
            .collect();
        assert_eq!(urls.len(), releases.len());
        assert!(urls.contains(
            "https://software.cisco.com/services/catalog/v1/releases?mdfid=286315874&softwareId=282046477#Dublin-17.12.8"
        ));
    }

    #[test]
    fn the_metadata_has_only_what_the_response_has() {
        let releases = parse_releases(C9300).unwrap();
        let (firmware_ref, meta) = to_firmware(
            release(&releases, "IOSXE-17.18.4"),
            &c9300(),
            "cisco",
            "catalyst-9300",
        );
        assert_eq!(firmware_ref.source_url, meta.source_url);
        assert_eq!(meta.vendor, "cisco");
        assert_eq!(meta.device_family, "catalyst-9300");
        assert_eq!(meta.hardware_targets, ["Catalyst 9300 Switch"]);
        assert_eq!(meta.version.raw, "IOSXE-17.18.4");
        assert_eq!(meta.release_date, None);
        assert_eq!(meta.sha256, None);
        assert_eq!(meta.release_notes_url, None);
    }

    /// Serves canned bodies by node, and counts the requests.
    struct Tree {
        bodies: HashMap<&'static str, String>,
        requests: std::sync::Mutex<Vec<String>>,
    }

    impl Tree {
        fn new(bodies: &[(&'static str, String)]) -> Self {
            Self {
                bodies: bodies.iter().cloned().collect(),
                requests: Default::default(),
            }
        }
    }

    #[async_trait]
    impl CatalogSource for Tree {
        async fn products(&self, mdf_id: &str) -> Result<String, PluginError> {
            self.requests.lock().unwrap().push(mdf_id.to_string());
            Ok(self
                .bodies
                .get(mdf_id)
                .cloned()
                .unwrap_or_else(|| r#"{"response":[]}"#.to_string()))
        }

        async fn software_types(&self, _: &str) -> Result<String, PluginError> {
            Err(PluginError::Unimplemented)
        }

        async fn releases(&self, _: &str, _: &str) -> Result<String, PluginError> {
            Err(PluginError::Unimplemented)
        }
    }

    fn node(id: &str, name: &str, parent: &str, leaf: bool) -> String {
        format!(
            r#"{{"mdfId":"{id}","metaClass":"Category","mdfConcept":"{name}","parentMdfId":"{parent}","leafNodeFlag":"{}"}}"#,
            if leaf { "Y" } else { "N" }
        )
    }

    fn level(nodes: &[String]) -> String {
        format!(r#"{{"response":[{}]}}"#, nodes.join(","))
    }

    /// root ─ Switches ─ Access ─ Catalyst A (leaf)
    ///      │                  └─ Catalyst B (leaf)
    ///      └ Routers (leaf)
    fn small_tree() -> Tree {
        Tree::new(&[
            (
                "1",
                level(&[
                    node("2", "Switches", "1", false),
                    node("9", "Routers", "1", true),
                ]),
            ),
            ("2", level(&[node("3", "Access", "2", false)])),
            (
                "3",
                level(&[
                    node("4", "Catalyst A", "3", true),
                    node("5", "Catalyst B", "3", true),
                ]),
            ),
        ])
    }

    #[tokio::test]
    async fn the_walk_returns_the_products_with_the_names_above_them() {
        let products = find_products(&small_tree(), "1", WalkLimits::default())
            .await
            .unwrap();
        let found: Vec<_> = products
            .iter()
            .map(|p| (p.node.mdf_id.as_str(), p.path.join(" / ")))
            .collect();
        assert_eq!(
            found,
            [
                ("9", "Routers".to_string()),
                ("4", "Switches / Access / Catalyst A".to_string()),
                ("5", "Switches / Access / Catalyst B".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn a_node_that_comes_up_twice_is_read_once() {
        let tree = Tree::new(&[
            (
                "1",
                level(&[node("2", "A", "1", false), node("3", "B", "1", false)]),
            ),
            ("2", level(&[node("4", "Shared", "2", false)])),
            ("3", level(&[node("4", "Shared", "3", false)])),
        ]);
        find_products(&tree, "1", WalkLimits::default())
            .await
            .unwrap();
        let requests = tree.requests.lock().unwrap();
        assert_eq!(requests.iter().filter(|id| *id == "4").count(), 1);
    }

    #[tokio::test]
    async fn a_walk_that_needs_more_requests_than_allowed_is_an_error() {
        let limits = WalkLimits {
            max_requests: 2,
            ..WalkLimits::default()
        };
        let tree = small_tree();
        let err = find_products(&tree, "1", limits).await.unwrap_err();
        assert!(err.to_string().contains("2 requests"), "{err}");
        assert_eq!(tree.requests.lock().unwrap().len(), 2, "no request past it");
    }

    #[tokio::test]
    async fn nodes_below_the_depth_limit_are_not_read() {
        let limits = WalkLimits {
            max_depth: 1,
            ..WalkLimits::default()
        };
        let tree = small_tree();
        let products = find_products(&tree, "1", limits).await.unwrap();
        assert_eq!(products.len(), 1, "only Routers is within one level");
        assert_eq!(*tree.requests.lock().unwrap(), ["1"]);
    }

    #[tokio::test]
    async fn the_walk_finds_nothing_in_the_sample_root_since_it_has_only_categories() {
        let tree = Tree::new(&[("268437593", PRODUCTS.to_string())]);
        let limits = WalkLimits {
            max_depth: 1,
            ..WalkLimits::default()
        };
        let products = find_products(&tree, "268437593", limits).await.unwrap();
        assert!(products.is_empty());
        assert_eq!(*tree.requests.lock().unwrap(), ["268437593"]);
    }
    const TYPES: &str = include_str!("../testdata/catalog-software-types-c9300.json");
    const SF220: &str = include_str!("../testdata/catalog-releases-sf220.json");

    #[test]
    fn the_software_types_url_takes_the_product_id() {
        assert_eq!(
            software_types_url("286315874").as_str(),
            "https://software.cisco.com/services/catalog/v1/softwareTypes?mdfid=286315874"
        );
    }

    #[test]
    fn the_sample_has_nine_software_types_each_with_a_numeric_id() {
        let types = parse_software_types(TYPES).unwrap();
        assert_eq!(types.len(), 9);
        let ios_xe = find_software_type(&types, "IOS XE Software").unwrap();
        assert_eq!(ios_xe.software_id, 282046477);
        assert_eq!(ios_xe.access_level, Some(1));
    }

    #[test]
    fn a_software_type_is_found_by_name_ignoring_case_and_space() {
        let types = parse_software_types(TYPES).unwrap();
        let found = find_software_type(&types, "  ios xe software ").unwrap();
        assert_eq!(found.software_id, 282046477);
        // Only the exact name: a prefix of another type's name is not it.
        assert_eq!(
            find_software_type(&types, "NBAR2 Protocol Packs")
                .unwrap()
                .software_id,
            284509011
        );
        assert!(find_software_type(&types, "Switch Firmware").is_none());
    }

    #[test]
    fn no_software_types_is_an_empty_list() {
        assert!(parse_software_types("{}").unwrap().is_empty());
        assert!(matches!(
            parse_software_types("<html>"),
            Err(PluginError::Parse(_))
        ));
    }

    #[test]
    fn the_sf220_has_sixteen_releases_none_deferred_or_suggested() {
        let releases = parse_releases(SF220).unwrap();
        assert_eq!(releases.len(), 16);
        assert!(releases
            .iter()
            .all(|r| r.availability == Availability::Available && !r.suggested));
    }

    #[test]
    fn the_sf220_has_no_train_name_and_no_life_cycle() {
        // bucket2 is empty and lifeCycle is "" on every entry, so there is
        // nothing to record (bucket1 is "1.2", "1", ... and isn't a train).
        let releases = parse_releases(SF220).unwrap();
        assert!(releases
            .iter()
            .all(|r| r.train.is_none() && r.life_cycle.is_none()));
    }

    #[test]
    fn the_sf220_versions_are_four_numbers_and_sort_numerically() {
        let releases = parse_releases(SF220).unwrap();
        assert!(releases
            .iter()
            .all(|r| matches!(version_key(&r.version).scheme, VersionScheme::VendorNumeric)));
        assert_eq!(version_key("1.2.1.5").ordinal, Some(vec![1, 2, 1, 5, 0]));
        let newest = releases
            .iter()
            .max_by_key(|r| version_key(&r.version).ordinal)
            .unwrap();
        assert_eq!(newest.version, "1.2.1.5");
        let oldest = releases
            .iter()
            .min_by_key(|r| version_key(&r.version).ordinal)
            .unwrap();
        assert_eq!(oldest.version, "1.0.0.17");
        // 18 before 19 as numbers, and 1.0.0.19 before 1.0.1.1.
        let order = |v: &str| version_key(v).ordinal;
        assert!(order("1.0.0.18") < order("1.0.0.19"));
        assert!(order("1.0.0.19") < order("1.0.1.1"));
        assert!(order("1.1.0.14") > order("1.0.1.1"));
    }

    /// Serves canned bodies and logs the requests as `kind:ids`.
    struct Catalog {
        products: HashMap<&'static str, String>,
        types: HashMap<&'static str, String>,
        releases: HashMap<(&'static str, &'static str), String>,
        requests: std::sync::Mutex<Vec<String>>,
    }

    impl Catalog {
        fn log(&self, entry: String) {
            self.requests.lock().unwrap().push(entry);
        }
    }

    #[async_trait]
    impl CatalogSource for Catalog {
        async fn products(&self, mdf_id: &str) -> Result<String, PluginError> {
            self.log(format!("products:{mdf_id}"));
            Ok(self.products.get(mdf_id).cloned().unwrap_or_default())
        }

        async fn software_types(&self, mdf_id: &str) -> Result<String, PluginError> {
            self.log(format!("types:{mdf_id}"));
            Ok(self.types.get(mdf_id).cloned().unwrap_or_default())
        }

        async fn releases(&self, mdf_id: &str, software_id: &str) -> Result<String, PluginError> {
            self.log(format!("releases:{mdf_id}:{software_id}"));
            Ok(self
                .releases
                .iter()
                .find(|((m, s), _)| *m == mdf_id && *s == software_id)
                .map(|(_, body)| body.clone())
                .unwrap_or_default())
        }
    }

    /// A tree with the two products of the samples below one category. The
    /// Catalyst 9300's software types and releases are the captured
    /// responses. The SF220 has its captured releases, but its node, ids and
    /// software types were never captured: the id `5000`, the software id `1`
    /// and the type name `Switch Firmware` are made up for this test.
    fn two_products() -> Catalog {
        Catalog {
            products: HashMap::from([
                ("1", level(&[node("2", "Switches", "1", false)])),
                (
                    "2",
                    level(&[
                        node("286315874", "Catalyst 9300 Switch", "2", true),
                        node("5000", "SF220-24", "2", true),
                    ]),
                ),
            ]),
            types: HashMap::from([
                ("286315874", TYPES.to_string()),
                (
                    "5000",
                    r#"{"softwareTypeList":[{"name":"Switch Firmware","softwareId":1}]}"#
                        .to_string(),
                ),
            ]),
            releases: HashMap::from([
                (("286315874", "282046477"), C9300.to_string()),
                (("5000", "1"), SF220.to_string()),
            ]),
            requests: Default::default(),
        }
    }

    #[tokio::test]
    async fn reading_a_subtree_reads_the_releases_of_the_products_that_have_the_software() {
        let catalog = two_products();
        let reading = read_catalog(&catalog, "1", "IOS XE Software", WalkLimits::default())
            .await
            .unwrap();

        assert_eq!(reading.read.len(), 1);
        let c9300 = &reading.read[0];
        assert_eq!(c9300.product.node.mdf_concept, "Catalyst 9300 Switch");
        assert_eq!(c9300.product.path, ["Switches", "Catalyst 9300 Switch"]);
        assert_eq!(c9300.software.software_id, 282046477);
        assert_eq!(c9300.releases.len(), 97);

        let without: Vec<_> = reading
            .without_software
            .iter()
            .map(|p| p.node.mdf_concept.as_str())
            .collect();
        assert_eq!(without, ["SF220-24"]);

        // The ids in the releases request are the ones the types gave.
        assert_eq!(
            *catalog.requests.lock().unwrap(),
            [
                "products:1",
                "products:2",
                "types:286315874",
                "releases:286315874:282046477",
                "types:5000",
            ]
        );
    }

    #[tokio::test]
    async fn the_same_read_with_another_software_name_gets_the_other_product() {
        let catalog = two_products();
        let reading = read_catalog(&catalog, "1", "Switch Firmware", WalkLimits::default())
            .await
            .unwrap();
        assert_eq!(reading.read.len(), 1);
        assert_eq!(reading.read[0].product.node.mdf_concept, "SF220-24");
        assert_eq!(reading.read[0].releases.len(), 16);
        assert_eq!(reading.without_software.len(), 1);
    }

    #[tokio::test]
    async fn the_request_limit_covers_the_walk_and_the_per_product_requests() {
        let catalog = two_products();
        let limits = WalkLimits {
            max_requests: 4,
            ..WalkLimits::default()
        };
        let err = read_catalog(&catalog, "1", "IOS XE Software", limits)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("more than 4 requests"), "{err}");
        assert_eq!(
            catalog.requests.lock().unwrap().len(),
            4,
            "the fifth request was not made"
        );
    }

    #[test]
    fn a_target_takes_its_ids_and_name_from_the_product_and_software_type() {
        let product = Product {
            node: parse_products(&level(&[node(
                "286315874",
                "Catalyst 9300 Switch",
                "2",
                true,
            )]))
            .unwrap()
            .remove(0),
            path: vec!["Catalyst 9300 Switch".into()],
        };
        let software = find_software_type(&parse_software_types(TYPES).unwrap(), "IOS XE Software")
            .unwrap()
            .clone();
        let target = Target::new(&product, &software);
        assert_eq!(target.mdf_id, "286315874");
        assert_eq!(target.software_id, "282046477");
        assert_eq!(target.name, "Catalyst 9300 Switch");
    }
}
