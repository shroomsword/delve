//! What `delve survey` collects, read from an existing config and written
//! back out as commented TOML.
//!
//! `Config` only derives `Deserialize`, and a written file should explain
//! itself, so the file is rendered by hand here rather than serialized. Every
//! rendered file must parse back into `Config` and into the same `Answers`,
//! which the tests check for each scenario.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use delve_core::context::SettingValue;

use crate::config::{Config, EmailTls, TransportKind};

/// Everything a config file can say that the survey knows about. `None` (or
/// an empty map) means "not set", which the file shows as a commented-out
/// default.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Answers {
    pub(crate) database_path: Option<String>,
    pub(crate) enabled: Vec<String>,
    /// Vendor id to key to value, as written: `env:NAME` or a literal.
    pub(crate) credentials: BTreeMap<String, BTreeMap<String, String>>,
    /// Vendor id to setting name to value.
    pub(crate) settings: BTreeMap<String, BTreeMap<String, SettingValue>>,
    pub(crate) transport_default: Option<TransportKind>,
    pub(crate) transport_overrides: BTreeMap<String, TransportKind>,
    pub(crate) user_agent: Option<String>,
    pub(crate) request_timeout_secs: Option<u64>,
    pub(crate) connect_timeout_secs: Option<u64>,
    pub(crate) min_request_interval_ms: Option<u64>,
    pub(crate) rate_limit_overrides: BTreeMap<String, u64>,
    pub(crate) changed_fields: Option<Vec<String>>,
    pub(crate) webhook_url: Option<String>,
    pub(crate) email: Option<EmailAnswers>,
}

/// `[subscribers.email]`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EmailAnswers {
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
    pub(crate) tls: EmailTls,
    pub(crate) from: String,
    pub(crate) to: Vec<String>,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) batch: bool,
    pub(crate) max_events_per_email: usize,
}

impl Answers {
    /// The answers an existing config gives. `default_database_path` is the
    /// path used when none is set, so a file that spells out the default
    /// reads the same as one that leaves it out.
    pub(crate) fn from_config(config: &Config, default_database_path: &str) -> Self {
        let sorted = |m: &std::collections::HashMap<String, String>| {
            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        Self {
            database_path: (config.database_path != default_database_path)
                .then(|| config.database_path.clone()),
            enabled: config.vendors.enabled.clone(),
            credentials: config
                .vendors
                .credentials
                .iter()
                .map(|(vendor, keys)| (vendor.clone(), sorted(keys)))
                .collect(),
            settings: config
                .vendors
                .settings
                .iter()
                .map(|(vendor, s)| {
                    (
                        vendor.clone(),
                        s.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    )
                })
                .collect(),
            transport_default: config.transport.default.clone(),
            transport_overrides: config
                .transport
                .overrides
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            user_agent: config.transport.user_agent.clone(),
            request_timeout_secs: config.transport.request_timeout_secs,
            connect_timeout_secs: config.transport.connect_timeout_secs,
            min_request_interval_ms: config.transport.min_request_interval_ms,
            rate_limit_overrides: config
                .transport
                .rate_limit_overrides
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            changed_fields: config.notifications.changed_fields.clone(),
            webhook_url: config.subscribers.webhook.as_ref().map(|w| w.url.clone()),
            email: config.subscribers.email.as_ref().map(|e| EmailAnswers {
                host: e.host.clone(),
                port: e.port,
                tls: e.tls,
                from: e.from.clone(),
                to: e.to.clone(),
                username: e.username.clone(),
                password: e.password.clone(),
                batch: e.batch,
                max_events_per_email: e.max_events_per_email,
            }),
        }
    }

    /// Every `env:` variable the answers refer to, with what refers to it,
    /// in the order they appear in the file.
    pub(crate) fn env_vars(&self) -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = Vec::new();
        let mut add = |value: &str, what: String| {
            if let Some(name) = value.strip_prefix("env:") {
                if !vars.iter().any(|(n, _)| n == name) {
                    vars.push((name.to_string(), what));
                }
            }
        };
        for (vendor, keys) in &self.credentials {
            for (key, value) in keys {
                add(value, format!("{vendor} credential '{key}'"));
            }
        }
        if let Some(email) = &self.email {
            for (field, value) in [("username", &email.username), ("password", &email.password)] {
                if let Some(value) = value {
                    add(value, format!("email {field}"));
                }
            }
        }
        vars
    }
}

/// A TOML string, quoted and escaped.
fn string(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// A TOML array of strings.
fn list<S: AsRef<str>>(items: &[S]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| string(s.as_ref())).collect();
    format!("[{}]", quoted.join(", "))
}

/// A table key: bare when TOML allows it, quoted otherwise.
fn key(k: &str) -> String {
    let bare = !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        k.to_string()
    } else {
        string(k)
    }
}

fn transport(kind: &TransportKind) -> String {
    match kind {
        TransportKind::Direct => string("direct"),
        TransportKind::Tor => string("tor"),
        TransportKind::Socks5 { addr } => format!("{{ socks5 = {{ addr = {} }} }}", string(addr)),
    }
}

fn setting(value: &SettingValue) -> String {
    match value {
        SettingValue::Text(t) => string(t),
        SettingValue::List(items) => list(items),
    }
}

/// `name = value`, or the default commented out when there is no value.
fn line_or_default(out: &mut String, name: &str, value: Option<String>, default: &str) {
    match value {
        Some(v) => {
            let _ = writeln!(out, "{name} = {v}");
        }
        None => {
            let _ = writeln!(out, "# {name} = {default}");
        }
    }
}

/// The config file for `answers`, with comments explaining each setting.
pub(crate) fn render(answers: &Answers, default_database_path: &str) -> String {
    let mut out = String::new();
    let o = &mut out;
    let _ = writeln!(
        o,
        "# delve configuration, written by `delve survey`. Edit it freely, or run\n\
         # `delve survey` again to change it. The README's \"Configuration reference\"\n\
         # describes every setting.\n"
    );

    let _ = writeln!(o, "# Where delve keeps what it has found.");
    line_or_default(
        o,
        "database_path",
        answers.database_path.as_deref().map(string),
        &string(default_database_path),
    );

    let _ = writeln!(o, "\n[vendors]");
    let _ = writeln!(
        o,
        "# Which compiled-in vendors `dig` runs. An empty list runs all of them."
    );
    let _ = writeln!(o, "enabled = {}", list(&answers.enabled));

    for (vendor, values) in &answers.settings {
        if values.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n[vendors.settings.{}]", key(vendor));
        for (name, value) in values {
            if let Some(comment) = setting_comment(vendor, name) {
                let _ = writeln!(o, "# {comment}");
            }
            let _ = writeln!(o, "{} = {}", key(name), setting(value));
        }
    }

    for (vendor, keys) in &answers.credentials {
        if keys.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n[vendors.credentials.{}]", key(vendor));
        let _ = writeln!(
            o,
            "# \"env:NAME\" reads the value from the environment variable NAME when delve\n\
             # runs, so no secret is stored in this file."
        );
        for (name, value) in keys {
            let _ = writeln!(o, "{} = {}", key(name), string(value));
        }
    }

    let _ = writeln!(o, "\n[transport]");
    let _ = writeln!(
        o,
        "# How delve reaches vendor sites: \"direct\", \"tor\" (a Tor daemon on\n\
         # 127.0.0.1:9050), or {{ socks5 = {{ addr = \"host:port\" }} }}."
    );
    line_or_default(
        o,
        "default",
        answers.transport_default.as_ref().map(transport),
        "\"direct\"",
    );
    let _ = writeln!(
        o,
        "# Identifies delve to vendor sites. Include a way to reach you, so a vendor\n\
         # can ask before blocking you."
    );
    line_or_default(
        o,
        "user_agent",
        answers.user_agent.as_deref().map(string),
        &string(&delve_core::context::default_user_agent()),
    );
    let _ = writeln!(o, "# Seconds one request, and one connection, may take.");
    line_or_default(
        o,
        "request_timeout_secs",
        answers.request_timeout_secs.map(|v| v.to_string()),
        "30",
    );
    line_or_default(
        o,
        "connect_timeout_secs",
        answers.connect_timeout_secs.map(|v| v.to_string()),
        "10",
    );
    let _ = writeln!(
        o,
        "# The fewest milliseconds between two requests to one vendor."
    );
    line_or_default(
        o,
        "min_request_interval_ms",
        answers.min_request_interval_ms.map(|v| v.to_string()),
        "1000",
    );

    if !answers.transport_overrides.is_empty() {
        let _ = writeln!(o, "\n[transport.overrides]");
        let _ = writeln!(o, "# A different transport for these vendors.");
        for (vendor, kind) in &answers.transport_overrides {
            let _ = writeln!(o, "{} = {}", key(vendor), transport(kind));
        }
    }
    if !answers.rate_limit_overrides.is_empty() {
        let _ = writeln!(o, "\n[transport.rate_limit_overrides]");
        let _ = writeln!(
            o,
            "# A different min_request_interval_ms for these vendors."
        );
        for (vendor, ms) in &answers.rate_limit_overrides {
            let _ = writeln!(o, "{} = {ms}", key(vendor));
        }
    }

    let _ = writeln!(o, "\n[notifications]");
    let _ = writeln!(
        o,
        "# New versions are always reported. These are the changes to a version\n\
         # already stored that are reported too: sha256, release_date,\n\
         # release_notes_url, signature, source_url."
    );
    line_or_default(
        o,
        "changed_fields",
        answers.changed_fields.as_deref().map(list),
        "[\"sha256\"]",
    );

    if let Some(url) = &answers.webhook_url {
        let _ = writeln!(o, "\n[subscribers.webhook]");
        let _ = writeln!(
            o,
            "# One JSON POST per dig; the README's \"Webhook\" section has the payload."
        );
        let _ = writeln!(o, "url = {}", string(url));
    }

    if let Some(email) = &answers.email {
        let _ = writeln!(o, "\n[subscribers.email]");
        let _ = writeln!(o, "host = {}", string(&email.host));
        let _ = writeln!(
            o,
            "# \"starttls\", \"implicit\" or \"none\" (a local relay only)."
        );
        let _ = writeln!(
            o,
            "tls = {}",
            string(match email.tls {
                EmailTls::StartTls => "starttls",
                EmailTls::Implicit => "implicit",
                EmailTls::None => "none",
            })
        );
        let default_port = match email.tls {
            EmailTls::StartTls => "587",
            EmailTls::Implicit => "465",
            EmailTls::None => "25",
        };
        line_or_default(o, "port", email.port.map(|p| p.to_string()), default_port);
        let _ = writeln!(o, "from = {}", string(&email.from));
        let _ = writeln!(o, "to = {}", list(&email.to));
        if email.username.is_some() || email.password.is_some() {
            let _ = writeln!(
                o,
                "# \"env:NAME\" reads the value from the environment when delve runs."
            );
        }
        if let Some(username) = &email.username {
            let _ = writeln!(o, "username = {}", string(username));
        }
        if let Some(password) = &email.password {
            let _ = writeln!(o, "password = {}", string(password));
        }
        let _ = writeln!(
            o,
            "# One message per dig, split past max_events_per_email; false sends one\n\
             # message per event."
        );
        let _ = writeln!(o, "batch = {}", email.batch);
        let _ = writeln!(o, "max_events_per_email = {}", email.max_events_per_email);
    }

    out
}

/// A comment for a vendor setting the survey knows about.
fn setting_comment(vendor: &str, name: &str) -> Option<&'static str> {
    match (vendor, name) {
        ("unifi", "products") => {
            Some("Which Ubiquiti products to track (README: \"Tracking console products\").")
        }
        ("unifi", "models") => Some(
            "Only these model codes, one request each (README: \"Tracking only some models\").\n\
             # Leave it out to track every model in one request per product.",
        ),
        _ => None,
    }
}
