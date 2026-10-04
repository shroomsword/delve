//! `delve survey` — asks questions and writes `config.toml`. See the README's
//! "Creating the config file" section.
//!
//! Archaeologists survey a site and plan the excavation before they dig; this
//! plans the `dig`. It runs only on a terminal (or with `--defaults`), starts
//! from the current file when there is one, checks the result with the same
//! code `dig` uses, and never replaces a file without asking.

mod answers;
mod prompt;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use delve_core::context::SettingValue;
use delve_core::events::WatchedField;

use crate::config::{Config, EmailTls, TransportKind};
use answers::{render, Answers, EmailAnswers};
use prompt::{Prompter, TerminalPrompter};

/// What this build offers: its vendors, and which optional sections exist.
#[derive(Debug, Clone)]
pub(crate) struct SurveyContext {
    /// Compiled-in vendors and whether their terms have been reviewed.
    pub(crate) vendors: Vec<(String, bool)>,
    /// UniFi's choices, when the UniFi vendor is compiled in.
    pub(crate) unifi: Option<UnifiChoices>,
    pub(crate) webhook: bool,
    pub(crate) email: bool,
    pub(crate) default_database_path: String,
}

#[derive(Debug, Clone)]
pub(crate) struct UnifiChoices {
    pub(crate) products: Vec<String>,
    pub(crate) default_products: Vec<String>,
    /// Product line and its model codes.
    pub(crate) lines: Vec<(String, Vec<String>)>,
}

impl SurveyContext {
    /// The context for this build of delve.
    fn for_this_build() -> Self {
        let registry = delve_core::plugin::PluginRegistry::discover();
        let mut vendors: Vec<(String, bool)> = registry
            .vendor_ids()
            .filter_map(|id| {
                registry
                    .get(id)
                    .map(|p| (id.to_string(), p.capabilities().tos_reviewed))
            })
            .collect();
        vendors.sort();
        Self {
            vendors,
            unifi: unifi_choices(),
            webhook: cfg!(feature = "subscriber-webhook"),
            email: cfg!(feature = "subscriber-email"),
            default_database_path: crate::config::default_db_path(),
        }
    }
}

#[cfg(feature = "vendor-unifi")]
fn unifi_choices() -> Option<UnifiChoices> {
    let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    Some(UnifiChoices {
        products: owned(vendor_unifi::SUPPORTED_PRODUCTS),
        default_products: owned(vendor_unifi::DEFAULT_PRODUCTS),
        lines: vendor_unifi::product_lines()
            .into_iter()
            .map(|(line, codes)| (line.to_string(), owned(&codes)))
            .collect(),
    })
}

#[cfg(not(feature = "vendor-unifi"))]
fn unifi_choices() -> Option<UnifiChoices> {
    None
}

/// Runs `delve survey`. `config_path` is `--config`, if given.
pub async fn run(config_path: Option<&Path>, print: bool, defaults: bool) -> anyhow::Result<()> {
    let path = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::config::default_config_path);
    let ctx = SurveyContext::for_this_build();

    if defaults {
        let mut stdout = std::io::stdout().lock();
        return write_defaults(&ctx, &path, print, &mut stdout);
    }
    ensure_terminal(std::io::stdin().is_terminal() && std::io::stderr().is_terminal())?;

    // The questions block on the terminal, so they run off the async runtime.
    let outcome = tokio::task::spawn_blocking(move || {
        let mut prompter = TerminalPrompter::new();
        let mut stdout = std::io::stdout().lock();
        survey(&mut prompter, &ctx, &path, print, &mut stdout)
    })
    .await??;

    send_tests(&outcome).await;
    Ok(())
}

/// The questions need someone to answer them.
fn ensure_terminal(is_terminal: bool) -> anyhow::Result<()> {
    if !is_terminal {
        anyhow::bail!(
            "delve survey asks questions, so it needs a terminal. For a starting file \
             without questions, run `delve survey --defaults --print > config.toml`, or see \
             the README's \"Configuration reference\""
        );
    }
    Ok(())
}

/// `--defaults`: the commented file with every default, and no questions.
fn write_defaults(
    ctx: &SurveyContext,
    path: &Path,
    print: bool,
    stdout: &mut dyn Write,
) -> anyhow::Result<()> {
    let rendered = render(&Answers::default(), &ctx.default_database_path);
    if print {
        stdout.write_all(rendered.as_bytes())?;
        return Ok(());
    }
    if path.exists() {
        anyhow::bail!(
            "{} already exists, and --defaults asks nothing, so it won't replace it. \
             Run `delve survey` to update it, or use --print",
            path.display()
        );
    }
    write_file(path, &rendered)?;
    writeln!(stdout, "Wrote {}", path.display())?;
    Ok(())
}

/// What a finished survey did, and which tests were asked for.
#[derive(Debug)]
struct Outcome {
    rendered: String,
    test_email: bool,
    test_webhook: bool,
}

/// Asks the questions, shows the file, and writes it (or prints it, with
/// `print`). Starts from the file at `path` when there is one.
fn survey(
    p: &mut dyn Prompter,
    ctx: &SurveyContext,
    path: &Path,
    print: bool,
    stdout: &mut dyn Write,
) -> anyhow::Result<Outcome> {
    let existing = existing_answers(p, ctx, path)?;
    let exists = path.exists();
    let mut answers = existing.clone().unwrap_or_default();

    let rendered = loop {
        ask_all(p, ctx, &mut answers, existing.as_ref())?;
        let rendered = render(&answers, &ctx.default_database_path);
        match check(&rendered) {
            Ok(()) => break rendered,
            Err(e) => {
                p.note(&format!("That config doesn't work: {e:#}"));
                if !p.confirm("Go through the questions again, with these answers?", true)? {
                    anyhow::bail!("nothing written: {e:#}");
                }
            }
        }
    };

    if print {
        stdout.write_all(rendered.as_bytes())?;
        return Ok(Outcome {
            rendered,
            test_email: false,
            test_webhook: false,
        });
    }

    p.note(&format!("\nHere is the file:\n\n{rendered}"));
    let question = if exists {
        format!(
            "Replace {}? (Its old contents are kept as {}; comments you added are not \
             carried over.)",
            path.display(),
            backup_path(path).display()
        )
    } else {
        format!("Write {}?", path.display())
    };
    if !p.confirm(&question, !exists)? {
        p.note("Nothing written.");
        return Ok(Outcome {
            rendered,
            test_email: false,
            test_webhook: false,
        });
    }
    if exists {
        std::fs::copy(path, backup_path(path))?;
    }
    write_file(path, &rendered)?;
    writeln!(stdout, "Wrote {}", path.display())?;
    env_var_reminder(p, &answers);

    let test_email =
        answers.email.is_some() && ctx.email && p.confirm("Send a test email now?", false)?;
    let test_webhook = answers.webhook_url.is_some()
        && ctx.webhook
        && p.confirm("Send a test webhook now?", false)?;
    Ok(Outcome {
        rendered,
        test_email,
        test_webhook,
    })
}

/// The answers in the file at `path`, if it exists. A file that doesn't
/// parse can be replaced, if the user agrees.
fn existing_answers(
    p: &mut dyn Prompter,
    ctx: &SurveyContext,
    path: &Path,
) -> anyhow::Result<Option<Answers>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            p.note(&format!(
                "This writes a new config file at {}.",
                path.display()
            ));
            return Ok(None);
        }
        Err(e) => anyhow::bail!("failed to read {}: {e}", path.display()),
    };
    match toml::from_str::<Config>(&text) {
        Ok(config) => {
            p.note(&format!(
                "Updating {}: each question starts from its current setting.",
                path.display()
            ));
            Ok(Some(Answers::from_config(
                &config,
                &ctx.default_database_path,
            )))
        }
        Err(e) => {
            p.note(&format!("{} doesn't parse: {e}", path.display()));
            if p.confirm("Start from scratch instead?", false)? {
                Ok(None)
            } else {
                anyhow::bail!("nothing written; fix {} or move it aside", path.display())
            }
        }
    }
}

/// Every question, in file order.
fn ask_all(
    p: &mut dyn Prompter,
    ctx: &SurveyContext,
    a: &mut Answers,
    before: Option<&Answers>,
) -> anyhow::Result<()> {
    ask_database(p, ctx, a)?;
    ask_vendors(p, ctx, a)?;
    ask_unifi(p, ctx, a, before)?;
    ask_credentials(p, a)?;
    ask_transport(p, a)?;
    ask_notifications(p, a)?;
    ask_webhook(p, ctx, a)?;
    ask_email(p, ctx, a)?;
    Ok(())
}

fn ask_database(p: &mut dyn Prompter, ctx: &SurveyContext, a: &mut Answers) -> anyhow::Result<()> {
    let current = a
        .database_path
        .clone()
        .unwrap_or_else(|| ctx.default_database_path.clone());
    let path = p.text("Database file", Some(&current), false)?;
    a.database_path = (path != ctx.default_database_path).then_some(path);
    Ok(())
}

fn ask_vendors(p: &mut dyn Prompter, ctx: &SurveyContext, a: &mut Answers) -> anyhow::Result<()> {
    if ctx.vendors.is_empty() {
        p.note("This build has no vendors compiled in; `dig` will have nothing to do.");
        return Ok(());
    }
    let labels: Vec<String> = ctx
        .vendors
        .iter()
        .map(|(id, reviewed)| {
            if *reviewed {
                id.clone()
            } else {
                format!("{id} (terms not reviewed yet: dig skips it)")
            }
        })
        .collect();
    let checked: Vec<bool> = ctx
        .vendors
        .iter()
        .map(|(id, reviewed)| {
            if a.enabled.is_empty() {
                *reviewed
            } else {
                a.enabled.contains(id)
            }
        })
        .collect();
    let chosen = p.multi_select("Vendors to dig", &labels, &checked)?;
    a.enabled = chosen
        .into_iter()
        .filter_map(|i| ctx.vendors.get(i).map(|(id, _)| id.clone()))
        .collect();
    for (id, reviewed) in &ctx.vendors {
        if !reviewed && a.enabled.contains(id) {
            p.note(&format!(
                "{id}'s terms haven't been reviewed, so `dig` skips it (see the README's \
                 \"Compliance\" section)."
            ));
        }
    }
    if a.enabled.is_empty() {
        p.note("No vendors chosen: an empty list means `dig` runs every compiled-in vendor.");
    }
    Ok(())
}

fn list_setting(a: &Answers, vendor: &str, name: &str) -> Option<Vec<String>> {
    match a.settings.get(vendor)?.get(name)? {
        SettingValue::List(items) => Some(items.clone()),
        SettingValue::Text(t) => Some(vec![t.clone()]),
    }
}

fn set_list(a: &mut Answers, vendor: &str, name: &str, value: Option<Vec<String>>) {
    let table = a.settings.entry(vendor.to_string()).or_default();
    match value {
        Some(items) => {
            table.insert(name.to_string(), SettingValue::List(items));
        }
        None => {
            table.remove(name);
        }
    }
    if table.is_empty() {
        a.settings.remove(vendor);
    }
}

fn ask_unifi(
    p: &mut dyn Prompter,
    ctx: &SurveyContext,
    a: &mut Answers,
    before: Option<&Answers>,
) -> anyhow::Result<()> {
    let Some(unifi) = &ctx.unifi else {
        return Ok(());
    };
    if !a.enabled.iter().any(|v| v == "unifi") {
        return Ok(());
    }

    p.note(
        "UniFi tracks network devices (access points, switches, gateways) by default. \
         The UniFi OS consoles are separate products.",
    );
    let current = list_setting(a, "unifi", "products").unwrap_or(unifi.default_products.clone());
    let checked: Vec<bool> = unifi.products.iter().map(|x| current.contains(x)).collect();
    let mut products: Vec<String> = p
        .multi_select("UniFi products", &unifi.products, &checked)?
        .into_iter()
        .filter_map(|i| unifi.products.get(i).cloned())
        .collect();
    if products.is_empty() {
        p.note("No products chosen; tracking the default.");
        products = unifi.default_products.clone();
    }
    let products_setting = (products != unifi.default_products).then_some(products);
    let products_before = before.and_then(|b| list_setting(b, "unifi", "products"));
    set_list(a, "unifi", "products", products_setting.clone());

    p.note(
        "Every model takes one request per product. Choosing models takes one request \
         per model, at one request a second, but stores and reports only those.",
    );
    let current_models = list_setting(a, "unifi", "models");
    let choices = vec![
        "Every model".to_string(),
        "Choose product lines".to_string(),
        "Enter model codes".to_string(),
    ];
    let models = match p.select(
        "Which UniFi models?",
        &choices,
        if current_models.is_some() { 2 } else { 0 },
    )? {
        0 => None,
        1 => {
            let labels: Vec<String> = unifi
                .lines
                .iter()
                .map(|(line, codes)| format!("{line} ({} models)", codes.len()))
                .collect();
            let checked: Vec<bool> = unifi
                .lines
                .iter()
                .map(|(_, codes)| {
                    current_models
                        .as_ref()
                        .is_some_and(|m| codes.iter().all(|c| m.contains(c)))
                })
                .collect();
            let mut codes: Vec<String> = p
                .multi_select("Product lines", &labels, &checked)?
                .into_iter()
                .filter_map(|i| unifi.lines.get(i))
                .flat_map(|(_, codes)| codes.iter().cloned())
                .collect();
            codes.sort();
            codes.dedup();
            (!codes.is_empty()).then_some(codes)
        }
        _ => {
            let current = current_models.clone().unwrap_or_default().join(", ");
            let text = p.text(
                "Model codes, separated by commas (as `catalog` shows them, e.g. U7PG2)",
                (!current.is_empty()).then_some(current.as_str()),
                false,
            )?;
            let codes: Vec<String> = text
                .split([',', ' '])
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(String::from)
                .collect();
            (!codes.is_empty()).then_some(codes)
        }
    };
    if let Some(codes) = &models {
        p.note(&format!(
            "{} models: a dig makes {} requests.",
            codes.len(),
            codes.len()
        ));
    }
    let models_before = before.and_then(|b| list_setting(b, "unifi", "models"));
    set_list(a, "unifi", "models", models.clone());

    if before.is_some() && (products_before != products_setting || models_before != models) {
        p.note(
            "You changed what UniFi tracks. The next dig would report the whole history of \
             anything new; run `delve dig --vendor unifi --redig` once to store it quietly.",
        );
    }
    Ok(())
}

/// Whether `name` can be an environment variable name.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether an answer looks like a secret pasted where a variable name was
/// asked for: long, with lower-case letters and digits mixed in, unlike the
/// usual `SOME_TOKEN` name.
fn looks_like_a_secret(value: &str) -> bool {
    value.len() >= 16
        && value.chars().any(|c| c.is_ascii_lowercase())
        && value.chars().any(|c| c.is_ascii_digit())
}

/// Asks for an environment variable name and returns `env:NAME`. Never takes
/// the secret itself.
fn ask_env_var(
    p: &mut dyn Prompter,
    prompt: &str,
    default: Option<&str>,
) -> anyhow::Result<String> {
    let default = default.and_then(|d| d.strip_prefix("env:"));
    loop {
        let answer = p.text(prompt, default, false)?;
        let name = answer.trim().strip_prefix("env:").unwrap_or(answer.trim());
        if !is_env_name(name) {
            p.note(
                "That isn't a variable name. Give the name of an environment variable that \
                 holds the value (like MY_TOKEN); delve never stores secrets in the file.",
            );
            continue;
        }
        if looks_like_a_secret(name)
            && !p.confirm(
                "That looks like a secret, not a variable name. Use it as a variable name \
                 anyway?",
                false,
            )?
        {
            continue;
        }
        return Ok(format!("env:{name}"));
    }
}

fn ask_credentials(p: &mut dyn Prompter, a: &mut Answers) -> anyhow::Result<()> {
    let mut credentials = BTreeMap::new();
    for vendor in a.enabled.clone() {
        let existing = a.credentials.get(&vendor).cloned().unwrap_or_default();
        let question = format!("Does {vendor} need credentials (a login or an API key)?");
        if !p.confirm(&question, !existing.is_empty())? {
            continue;
        }
        let mut keys = BTreeMap::new();
        for (key, value) in &existing {
            if !value.starts_with("env:") {
                p.note(&format!(
                    "{vendor}'s '{key}' is written in the file as a literal value. It's \
                     safer in an environment variable."
                ));
            }
            let env = ask_env_var(
                p,
                &format!("Environment variable for {vendor}'s '{key}'"),
                Some(value),
            )?;
            keys.insert(key.clone(), env);
        }
        loop {
            let ask_more = if keys.is_empty() {
                true
            } else {
                p.confirm(&format!("Add another credential for {vendor}?"), false)?
            };
            if !ask_more {
                break;
            }
            let key = p.text(
                &format!("Credential name for {vendor} (the README's vendor section lists them)"),
                None,
                false,
            )?;
            let env = ask_env_var(
                p,
                &format!("Environment variable for {vendor}'s '{key}'"),
                None,
            )?;
            keys.insert(key, env);
        }
        credentials.insert(vendor, keys);
    }
    a.credentials = credentials;
    Ok(())
}

fn transport_choices() -> Vec<String> {
    vec![
        "direct".to_string(),
        "tor (a Tor daemon on 127.0.0.1:9050)".to_string(),
        "socks5 (a SOCKS5 proxy)".to_string(),
    ]
}

fn ask_transport_kind(
    p: &mut dyn Prompter,
    prompt: &str,
    current: Option<&TransportKind>,
) -> anyhow::Result<TransportKind> {
    let default = match current {
        None | Some(TransportKind::Direct) => 0,
        Some(TransportKind::Tor) => 1,
        Some(TransportKind::Socks5 { .. }) => 2,
    };
    Ok(match p.select(prompt, &transport_choices(), default)? {
        0 => TransportKind::Direct,
        1 => TransportKind::Tor,
        _ => {
            let current = match current {
                Some(TransportKind::Socks5 { addr }) => Some(addr.as_str()),
                _ => None,
            };
            loop {
                let addr = p.text("SOCKS5 proxy address (host:port)", current, false)?;
                if addr.parse::<std::net::SocketAddr>().is_ok() {
                    break TransportKind::Socks5 { addr };
                }
                p.note("That isn't an address like 127.0.0.1:1080.");
            }
        }
    })
}

fn ask_number(p: &mut dyn Prompter, prompt: &str, default: u64) -> anyhow::Result<u64> {
    loop {
        let text = p.text(prompt, Some(&default.to_string()), false)?;
        match text.trim().parse::<u64>() {
            Ok(n) if n > 0 => return Ok(n),
            _ => p.note("Enter a whole number above zero."),
        }
    }
}

fn ask_transport(p: &mut dyn Prompter, a: &mut Answers) -> anyhow::Result<()> {
    let default = ask_transport_kind(
        p,
        "How should delve reach vendor sites?",
        a.transport_default.as_ref(),
    )?;
    a.transport_default = (default != TransportKind::Direct).then_some(default.clone());

    let mut overrides = BTreeMap::new();
    if !a.enabled.is_empty()
        && p.confirm(
            "Use a different transport for some vendors?",
            !a.transport_overrides.is_empty(),
        )?
    {
        for vendor in &a.enabled {
            let current = a.transport_overrides.get(vendor).unwrap_or(&default);
            let kind = ask_transport_kind(p, &format!("Transport for {vendor}"), Some(current))?;
            if kind != default {
                overrides.insert(vendor.clone(), kind);
            }
        }
    }
    a.transport_overrides = overrides;

    p.note(
        "Vendor sites see a User-Agent. One with a way to reach you lets a vendor ask \
         you to slow down instead of blocking you.",
    );
    a.user_agent = match a.user_agent.clone() {
        Some(current) => Some(p.text("User-Agent", Some(&current), false)?),
        None => {
            let contact = p.text(
                "Contact for the User-Agent (an email address or URL; empty to skip)",
                None,
                true,
            )?;
            let contact = contact.trim();
            if contact.is_empty() {
                p.note("Leaving the User-Agent at its default, with no contact.");
                None
            } else {
                Some(format!(
                    "{} (contact: {contact})",
                    delve_core::context::default_user_agent()
                ))
            }
        }
    };

    p.note(
        "delve waits between requests to one vendor. Check a vendor's robots.txt \
         Crawl-delay before going faster than one a second.",
    );
    let global = ask_number(
        p,
        "Milliseconds between requests to one vendor",
        a.min_request_interval_ms.unwrap_or(1000),
    )?;
    a.min_request_interval_ms = (global != 1000).then_some(global);

    let mut rates = BTreeMap::new();
    if !a.enabled.is_empty()
        && p.confirm(
            "Use a different rate for some vendors?",
            !a.rate_limit_overrides.is_empty(),
        )?
    {
        for vendor in &a.enabled {
            let current = a
                .rate_limit_overrides
                .get(vendor)
                .copied()
                .unwrap_or(global);
            let ms = ask_number(
                p,
                &format!("Milliseconds between requests to {vendor}"),
                current,
            )?;
            if ms != global {
                rates.insert(vendor.clone(), ms);
            }
        }
    }
    a.rate_limit_overrides = rates;
    Ok(())
}

fn ask_notifications(p: &mut dyn Prompter, a: &mut Answers) -> anyhow::Result<()> {
    let names: Vec<String> = WatchedField::ALL
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    let current = a
        .changed_fields
        .clone()
        .unwrap_or_else(|| vec!["sha256".to_string()]);
    let checked: Vec<bool> = names.iter().map(|n| current.contains(n)).collect();
    let chosen: Vec<String> = p
        .multi_select(
            "New versions are always reported. Which changes to a known version should be too?",
            &names,
            &checked,
        )?
        .into_iter()
        .filter_map(|i| names.get(i).cloned())
        .collect();
    a.changed_fields = (chosen != ["sha256"]).then_some(chosen);
    Ok(())
}

fn ask_webhook(p: &mut dyn Prompter, ctx: &SurveyContext, a: &mut Answers) -> anyhow::Result<()> {
    if !ctx.webhook {
        if a.webhook_url.is_some() {
            p.note("This build has no webhook support; the [subscribers.webhook] section is kept.");
        }
        return Ok(());
    }
    if !p.confirm(
        "Post each dig's changes to a webhook?",
        a.webhook_url.is_some(),
    )? {
        a.webhook_url = None;
        return Ok(());
    }
    loop {
        let url = p.text("Webhook URL", a.webhook_url.as_deref(), false)?;
        match url::Url::parse(url.trim()) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => {
                a.webhook_url = Some(url.trim().to_string());
                return Ok(());
            }
            _ => p.note("That isn't an http:// or https:// URL."),
        }
    }
}

/// An email service with known settings, from the README's "Choosing an
/// email service".
fn email_preset(host: &str) -> usize {
    match host {
        "smtp.postmarkapp.com" => 0,
        "smtp.resend.com" => 1,
        h if h.starts_with("email-smtp.") && h.ends_with(".amazonaws.com") => 2,
        _ => 3,
    }
}

fn ask_email(p: &mut dyn Prompter, ctx: &SurveyContext, a: &mut Answers) -> anyhow::Result<()> {
    if !ctx.email {
        if a.email.is_some() {
            p.note("This build has no email support; the [subscribers.email] section is kept.");
        }
        return Ok(());
    }
    if !p.confirm("Email each dig's changes?", a.email.is_some())? {
        a.email = None;
        return Ok(());
    }
    let current = a.email.clone();
    let current_host = current.as_ref().map(|e| e.host.as_str()).unwrap_or("");
    p.note(
        "Use a service made for sending program mail, with credentials that can only \
         send (the README's \"Choosing an email service\").",
    );
    let services = vec![
        "Postmark".to_string(),
        "Resend".to_string(),
        "Amazon SES".to_string(),
        "Another SMTP server".to_string(),
    ];
    let preset = p.select("Email service", &services, email_preset(current_host))?;
    let keep = |field: Option<&String>, preset_value: &str| -> String {
        field.cloned().unwrap_or_else(|| preset_value.to_string())
    };
    let (host, tls, port, username, password) = match preset {
        0 => {
            let token = ask_env_var(
                p,
                "Environment variable holding the Postmark Server API Token",
                Some(&keep(
                    current.as_ref().and_then(|e| e.password.as_ref()),
                    "env:POSTMARK_SERVER_TOKEN",
                )),
            )?;
            (
                "smtp.postmarkapp.com".to_string(),
                EmailTls::StartTls,
                Some(587),
                Some(token.clone()),
                Some(token),
            )
        }
        1 => {
            let key = ask_env_var(
                p,
                "Environment variable holding the Resend API key",
                Some(&keep(
                    current.as_ref().and_then(|e| e.password.as_ref()),
                    "env:RESEND_API_KEY",
                )),
            )?;
            (
                "smtp.resend.com".to_string(),
                EmailTls::StartTls,
                Some(587),
                Some("resend".to_string()),
                Some(key),
            )
        }
        2 => {
            let current_region = current_host
                .strip_prefix("email-smtp.")
                .and_then(|h| h.strip_suffix(".amazonaws.com"))
                .unwrap_or("us-east-1")
                .to_string();
            let region = loop {
                let r = p.text("AWS region", Some(&current_region), false)?;
                if !r.is_empty()
                    && r.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    break r;
                }
                p.note("That isn't a region like us-east-1.");
            };
            let user = ask_env_var(
                p,
                "Environment variable holding the SES SMTP username",
                Some(&keep(
                    current.as_ref().and_then(|e| e.username.as_ref()),
                    "env:SES_SMTP_USERNAME",
                )),
            )?;
            let pass = ask_env_var(
                p,
                "Environment variable holding the SES SMTP password",
                Some(&keep(
                    current.as_ref().and_then(|e| e.password.as_ref()),
                    "env:SES_SMTP_PASSWORD",
                )),
            )?;
            (
                format!("email-smtp.{region}.amazonaws.com"),
                EmailTls::StartTls,
                Some(587),
                Some(user),
                Some(pass),
            )
        }
        _ => {
            let host = p.text(
                "SMTP server",
                (!current_host.is_empty()).then_some(current_host),
                false,
            )?;
            let modes = vec![
                "starttls (port 587)".to_string(),
                "implicit TLS (port 465)".to_string(),
                "none (a local relay only: no encryption)".to_string(),
            ];
            let tls_default = match current.as_ref().map(|e| e.tls) {
                Some(EmailTls::Implicit) => 1,
                Some(EmailTls::None) => 2,
                _ => 0,
            };
            let tls = match p.select("Connection security", &modes, tls_default)? {
                0 => EmailTls::StartTls,
                1 => EmailTls::Implicit,
                _ => EmailTls::None,
            };
            let usual = match tls {
                EmailTls::StartTls => 587,
                EmailTls::Implicit => 465,
                EmailTls::None => 25,
            };
            let port = ask_number(
                p,
                "Port",
                current
                    .as_ref()
                    .and_then(|e| e.port)
                    .map_or(usual, u64::from),
            )?;
            let port = u16::try_from(port).ok();
            let has_login = current.as_ref().is_some_and(|e| e.username.is_some());
            let (username, password) = if p.confirm(
                "Does the server need a login?",
                has_login || current.is_none(),
            )? {
                let user = p.text(
                    "Username (as written, or env:NAME to read it from the environment)",
                    current.as_ref().and_then(|e| e.username.as_deref()),
                    false,
                )?;
                let pass = ask_env_var(
                    p,
                    "Environment variable holding the password",
                    current.as_ref().and_then(|e| e.password.as_deref()),
                )?;
                (Some(user), Some(pass))
            } else {
                (None, None)
            };
            (host, tls, port, username, password)
        }
    };

    let from = p.text(
        "From address (a bare address or \"Name <address>\"; the service must have it verified)",
        current.as_ref().map(|e| e.from.as_str()),
        false,
    )?;
    let to_current = current.as_ref().map(|e| e.to.join(", "));
    let to_text = p.text("To, separated by commas", to_current.as_deref(), false)?;
    let to: Vec<String> = to_text
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();
    let batch = p.confirm(
        "Send one message per dig instead of one per change?",
        current.as_ref().is_none_or(|e| e.batch),
    )?;
    let max = if batch {
        let n = ask_number(
            p,
            "Most changes in one message (a bigger dig is split)",
            current
                .as_ref()
                .map_or(50, |e| e.max_events_per_email as u64),
        )?;
        usize::try_from(n).unwrap_or(50)
    } else {
        current.as_ref().map_or(50, |e| e.max_events_per_email)
    };

    // The presets name the provider's usual port; keep it out of the file
    // when it is the TLS mode's default anyway.
    let usual = match tls {
        EmailTls::StartTls => 587,
        EmailTls::Implicit => 465,
        EmailTls::None => 25,
    };
    a.email = Some(EmailAnswers {
        host,
        port: port.filter(|p| *p != usual),
        tls,
        from: from.trim().to_string(),
        to,
        username,
        password,
        batch,
        max_events_per_email: max,
    });
    Ok(())
}

/// Checks a rendered file with the same code `dig` uses: it parses, the
/// change policy and transports resolve, and the email settings are sound.
/// Environment variables aren't needed: unset ones are reported afterwards.
fn check(rendered: &str) -> anyhow::Result<()> {
    let config: Config = toml::from_str(rendered)?;
    config.notifications.change_policy()?;
    config.transport.resolve()?;
    if let Some(email) = &config.subscribers.email {
        email.max_events()?;
        match (&email.username, &email.password) {
            (Some(_), None) => anyhow::bail!("the email login has a username but no password"),
            (None, Some(_)) => anyhow::bail!("the email login has a password but no username"),
            _ => {}
        }
        #[cfg(feature = "subscriber-email")]
        subscriber_email::EmailSubscriber::with_transport((), &email.from, &email.to)?;
    }
    if let Some(webhook) = &config.subscribers.webhook {
        url::Url::parse(&webhook.url)?;
    }
    Ok(())
}

/// Reminds the user to set every variable the file refers to.
fn env_var_reminder(p: &mut dyn Prompter, a: &Answers) {
    let vars = a.env_vars();
    if vars.is_empty() {
        return;
    }
    let mut text = String::from(
        "\nThe file reads these from the environment when delve runs; set them before `dig`:\n",
    );
    for (name, what) in vars {
        let state = if std::env::var_os(&name).is_some() {
            "set"
        } else {
            "not set"
        };
        text.push_str(&format!("  export {name}=...   # {what} ({state} now)\n"));
    }
    p.note(&text);
}

/// `config.toml.bak` next to `config.toml`.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// Writes the file, creating its directory, readable only by its owner on
/// Unix (it names where secrets live, even though it holds none).
fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    #[cfg(unix)]
    {
        // `mode` only applies to a new file; an existing one keeps its mode.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// A clearly fake release, for the test email and webhook.
fn test_event() -> delve_core::events::FirmwareEvent {
    use delve_core::model::{FirmwareMetadata, VersionKey};
    delve_core::events::FirmwareEvent::NewRelease {
        firmware: FirmwareMetadata {
            vendor: "delve".into(),
            device_family: "survey-test".into(),
            source_url: url::Url::parse("https://example.invalid/delve-survey-test")
                .unwrap_or_else(|_| unreachable!("a constant URL parses")),
            version: VersionKey::opaque("0.0.0-test"),
            release_date: Some(chrono::Utc::now().date_naive()),
            sha256: None,
            signature: None,
            hardware_targets: vec!["test".into()],
            release_notes_url: None,
            display_name: Some("delve survey test message".into()),
        },
        first_seen: chrono::Utc::now(),
    }
}

/// Sends the test email and webhook that were asked for, through the same
/// subscribers `dig` builds, and says how each went.
async fn send_tests(outcome: &Outcome) {
    if !outcome.test_email && !outcome.test_webhook {
        return;
    }
    let config: Config = match toml::from_str(&outcome.rendered) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("can't send tests: {e}");
            return;
        }
    };
    let event = [test_event()];
    #[cfg(feature = "subscriber-email")]
    if outcome.test_email {
        if let Some(email) = &config.subscribers.email {
            match crate::commands::dig::email_subscriber(email) {
                Ok(subscriber) => match subscriber.notify(&event).await {
                    Ok(()) => eprintln!("Test email sent to {}.", email.to.join(", ")),
                    Err(e) => eprintln!("The test email failed: {e}"),
                },
                Err(e) => eprintln!("Can't send a test email: {e:#}"),
            }
        }
    }
    #[cfg(feature = "subscriber-webhook")]
    if outcome.test_webhook {
        if let Some(webhook) = &config.subscribers.webhook {
            use delve_core::events::Subscriber;
            let subscriber = subscriber_webhook::WebhookSubscriber::new(webhook.url.clone());
            match subscriber.notify(&event).await {
                Ok(()) => eprintln!("Test webhook posted to {}.", webhook.url),
                Err(e) => eprintln!("The test webhook failed: {e}"),
            }
        }
    }
    let _ = (&config, &event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use prompt::{Answer, Answer::*, ScriptedPrompter};

    fn ctx() -> SurveyContext {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        SurveyContext {
            vendors: vec![("cisco".into(), false), ("unifi".into(), true)],
            unifi: Some(UnifiChoices {
                products: owned(&["unifi-firmware", "unifi-dream", "unifi-nvr"]),
                default_products: owned(&["unifi-firmware"]),
                lines: vec![
                    ("UAP".into(), owned(&["U7LT", "U7PG2"])),
                    ("USW".into(), owned(&["US8", "USMINI"])),
                ],
            }),
            webhook: true,
            email: true,
            // The real default: parsing a file fills it in, so round trips
            // need the same one. Golden files show it as DEFAULT_DB.
            default_database_path: crate::config::default_db_path(),
        }
    }

    /// The default database path as the file quotes it, and the placeholder
    /// golden files use instead, so they read the same on every machine.
    fn default_db_quoted() -> (String, &'static str) {
        (
            toml::Value::String(crate::config::default_db_path()).to_string(),
            "\"DEFAULT_DB\"",
        )
    }

    /// A fresh directory for one test's config file.
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("delve-survey-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Run {
        result: anyhow::Result<Outcome>,
        stdout: String,
        transcript: String,
        file: Option<String>,
        path: PathBuf,
        finished: bool,
    }

    /// Runs the survey with `script`, over `existing` if given.
    fn run_script(script: Vec<Answer>, existing: Option<&str>, print: bool) -> Run {
        let path = temp_dir().join("delve").join("config.toml");
        if let Some(text) = existing {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        let mut prompter = ScriptedPrompter::new(script);
        let mut stdout = Vec::new();
        let result = survey(&mut prompter, &ctx(), &path, print, &mut stdout);
        Run {
            result,
            stdout: String::from_utf8(stdout).unwrap(),
            transcript: prompter.transcript.join("\n"),
            file: std::fs::read_to_string(&path).ok(),
            finished: prompter.finished(),
            path,
        }
    }

    /// The answers that take UniFi with every model, no credentials, a
    /// direct connection with a contact address, and the other defaults, up
    /// to the subscriber questions.
    fn basics() -> Vec<Answer> {
        vec![
            Default,                  // database file
            Check(&["unifi"]),        // vendors
            Default,                  // UniFi products
            Pick("Every model"),      // UniFi models
            No,                       // unifi credentials
            Pick("direct"),           // transport
            No,                       // per-vendor transport
            Text("ops@example.test"), // User-Agent contact
            Default,                  // rate
            No,                       // per-vendor rate
            Default,                  // changed fields
        ]
    }

    fn script(parts: &[&[Answer]]) -> Vec<Answer> {
        parts.iter().flat_map(|p| p.iter().cloned()).collect()
    }

    /// Compares `actual` with `testdata/<name>.toml`; `UPDATE_GOLDEN=1`
    /// rewrites the file instead.
    fn golden(name: &str, actual: &str) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/commands/survey/testdata")
            .join(format!("{name}.toml"));
        let (quoted, placeholder) = default_db_quoted();
        let actual = actual.replace(&quoted, placeholder);
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &actual).unwrap();
            return;
        }
        // A Windows checkout may turn the file's newlines into CRLF; the
        // survey writes LF everywhere.
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (run with UPDATE_GOLDEN=1)", path.display()))
            .replace("\r\n", "\n");
        assert_eq!(actual, expected, "{name} differs from {}", path.display());
    }

    /// A written file parses into `Config`, and reading it back and writing
    /// it again gives the same file.
    fn round_trips(file: &str) {
        let config: Config = toml::from_str(file).expect("the written file parses");
        let again = render(
            &Answers::from_config(&config, &ctx().default_database_path),
            &ctx().default_database_path,
        );
        assert_eq!(again, file, "re-rendering changed the file");
        check(file).expect("the written file passes the same checks");
    }

    /// Runs a scenario that ends by writing the file, and checks it.
    fn scenario(name: &str, script: Vec<Answer>) -> Run {
        let run = run_script(script, None, false);
        assert!(run.result.is_ok(), "{:?}\n{}", run.result, run.transcript);
        assert!(run.finished, "unused answers left\n{}", run.transcript);
        let file = run.file.clone().expect("a file was written");
        round_trips(&file);
        golden(name, &file);
        run
    }

    #[test]
    fn minimal_unifi_with_a_contact() {
        let run = scenario("minimal", script(&[&basics(), &[No, No, Yes]]));
        assert!(run.stdout.contains("Wrote "), "{}", run.stdout);
        assert!(
            run.transcript.contains("This writes a new config file"),
            "{}",
            run.transcript
        );
    }

    #[test]
    fn unifi_lines_tor_overrides_credentials_and_a_webhook() {
        let run = scenario(
            "lines-tor-webhook",
            vec![
                Text("/srv/delve/delve.sqlite"),
                Check(&["cisco", "unifi"]),
                Check(&["unifi-firmware", "unifi-dream"]),
                Pick("Choose product lines"),
                Check(&["USW"]),
                // cisco credentials, including a pasted secret that is refused.
                Yes,
                Text("client_id"),
                Text("CISCO_CLIENT_ID"),
                Yes,
                Text("client_secret"),
                Text("sk4f9a8b7c6d5e4f3a2b1c0d"),
                No,
                Text("CISCO_CLIENT_SECRET"),
                No,
                No, // unifi credentials
                Pick("tor"),
                Yes,
                Pick("direct"), // cisco
                Default,        // unifi: same as the default
                Text("https://example.test/contact"),
                Text("2000"),
                Yes,
                Text("5000"), // cisco
                Default,      // unifi
                Check(&["sha256", "release_notes_url"]),
                Yes,
                Text("not a url"),
                Text("https://hooks.example.test/delve"),
                No, // email
                Yes,
                No, // test webhook
            ],
        );
        assert!(
            run.transcript.contains("terms haven't been reviewed"),
            "{}",
            run.transcript
        );
        assert!(
            run.transcript.contains("looks like a secret"),
            "{}",
            run.transcript
        );
        assert!(
            run.transcript.contains("isn't an http"),
            "{}",
            run.transcript
        );
        assert!(
            run.transcript.contains("export CISCO_CLIENT_SECRET=..."),
            "{}",
            run.transcript
        );
        let file = run.file.unwrap();
        assert!(
            !file.contains("sk4f9a8b7c6d5e4f3a2b1c0d"),
            "the secret is never written"
        );
    }

    #[test]
    fn email_through_postmark() {
        scenario(
            "email-postmark",
            script(&[
                &basics(),
                &[
                    No,
                    Yes,
                    Pick("Postmark"),
                    Default,
                    Text("Delve <delve@example.test>"),
                    Text("ops@example.test, sec@example.test"),
                    Default,
                    Default,
                    Yes,
                    No,
                ],
            ]),
        );
    }

    #[test]
    fn email_through_resend() {
        scenario(
            "email-resend",
            script(&[
                &basics(),
                &[
                    No,
                    Yes,
                    Pick("Resend"),
                    Default,
                    Text("delve@example.test"),
                    Text("ops@example.test"),
                    Default,
                    Text("20"),
                    Yes,
                    No,
                ],
            ]),
        );
    }

    #[test]
    fn email_through_ses_unbatched() {
        scenario(
            "email-ses",
            script(&[
                &basics(),
                &[
                    No,
                    Yes,
                    Pick("Amazon SES"),
                    Text("eu-west-1"),
                    Default,
                    Default,
                    Text("delve@example.test"),
                    Text("ops@example.test"),
                    No, // one message per change: no cap question
                    Yes,
                    No,
                ],
            ]),
        );
    }

    #[test]
    fn email_through_a_local_relay_without_a_login() {
        scenario(
            "email-relay",
            script(&[
                &basics(),
                &[
                    No,
                    Yes,
                    Pick("Another SMTP server"),
                    Text("relay.example.test"),
                    Pick("none"),
                    Default, // port 25
                    No,      // login
                    Text("delve@example.test"),
                    Text("ops@example.test"),
                    Default,
                    Default,
                    Yes,
                    No,
                ],
            ]),
        );
    }

    #[test]
    fn the_defaults_template() {
        let mut out = Vec::new();
        let path = temp_dir().join("config.toml");
        write_defaults(&ctx(), &path, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!path.exists(), "--print writes nothing to disk");
        round_trips(&text);
        golden("defaults", &text);
    }

    #[test]
    fn defaults_writes_a_new_file_but_never_replaces_one() {
        let path = temp_dir().join("sub").join("config.toml");
        let mut out = Vec::new();
        write_defaults(&ctx(), &path, false, &mut out).unwrap();
        assert!(path.exists());
        let before = std::fs::read_to_string(&path).unwrap();

        let err = write_defaults(&ctx(), &path, false, &mut out).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let run = scenario("minimal", script(&[&basics(), &[No, No, Yes]]));
        let mode = std::fs::metadata(&run.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[test]
    fn without_a_terminal_it_says_how_to_get_a_file_instead() {
        let err = ensure_terminal(false).unwrap_err().to_string();
        assert!(err.contains("needs a terminal"), "{err}");
        assert!(err.contains("--defaults --print"), "{err}");
        assert!(ensure_terminal(true).is_ok());
    }

    #[test]
    fn print_writes_the_file_to_stdout_and_nothing_to_disk() {
        let run = run_script(script(&[&basics(), &[No, No]]), None, true);
        run.result.unwrap();
        assert!(run.file.is_none(), "nothing on disk");
        round_trips(&run.stdout);
        assert!(run.finished, "{}", run.transcript);
    }

    #[test]
    fn declining_writes_nothing() {
        let run = run_script(script(&[&basics(), &[No, No, No]]), None, false);
        run.result.unwrap();
        assert!(run.file.is_none());
        assert!(
            run.transcript.contains("Nothing written."),
            "{}",
            run.transcript
        );
    }

    /// The file the Tor scenario writes, as an existing config.
    fn existing_file() -> String {
        let (quoted, placeholder) = default_db_quoted();
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/commands/survey/testdata/lines-tor-webhook.toml"),
        )
        .unwrap()
        .replace("\r\n", "\n")
        .replace(placeholder, &quoted)
    }

    /// The answers that keep every setting of `existing_file`.
    fn keep_everything() -> Vec<Answer> {
        vec![
            Default, // database file
            Default, // vendors
            Default, // UniFi products
            Default, // UniFi models: enter codes, prefilled
            Default, // the codes
            Default, // cisco credentials: yes
            Default, // client_id
            Default, // client_secret
            Default, // add another: no
            Default, // unifi credentials: no
            Default, // transport
            Default, // per-vendor transport: yes
            Default, // cisco
            Default, // unifi
            Default, // User-Agent
            Default, // rate
            Default, // per-vendor rate: yes
            Default, // cisco
            Default, // unifi
            Default, // changed fields
            Default, // webhook: yes
            Default, // URL
            Default, // email: no
        ]
    }

    #[test]
    fn an_existing_file_is_the_starting_point_and_replaced_only_after_asking() {
        let existing = existing_file();
        let run = run_script(
            script(&[&keep_everything(), &[Yes, No]]),
            Some(&existing),
            false,
        );
        run.result.unwrap();
        assert!(run.finished, "{}", run.transcript);
        assert!(run.transcript.contains("Updating "), "{}", run.transcript);
        assert!(run.transcript.contains("Replace "), "{}", run.transcript);
        // Keeping every answer writes the same file, and the old one is kept.
        assert_eq!(run.file.unwrap(), existing);
        let backup = std::fs::read_to_string(backup_path(&run.path)).unwrap();
        assert_eq!(backup, existing);
        // Nothing about what UniFi tracks changed, so no --redig advice.
        assert!(!run.transcript.contains("--redig"), "{}", run.transcript);
    }

    #[test]
    fn an_existing_file_is_kept_when_replacing_is_declined() {
        let existing = existing_file();
        let run = run_script(
            script(&[&keep_everything(), &[Default]]),
            Some(&existing),
            false,
        );
        run.result.unwrap();
        assert_eq!(run.file.unwrap(), existing, "replacing defaults to no");
        assert!(!backup_path(&run.path).exists());
    }

    #[test]
    fn changing_what_unifi_tracks_suggests_a_redig() {
        let mut answers = keep_everything();
        answers[4] = Text("USMINI"); // a different set of models
        let run = run_script(
            script(&[&answers, &[Yes, No]]),
            Some(&existing_file()),
            false,
        );
        run.result.unwrap();
        assert!(run.transcript.contains("--redig"), "{}", run.transcript);
        assert!(run.file.unwrap().contains("models = [\"USMINI\"]"));
    }

    #[test]
    fn a_file_that_does_not_parse_is_left_alone_unless_starting_over() {
        let broken = "this is = = not toml";
        let run = run_script(vec![No], Some(broken), false);
        assert!(run
            .result
            .unwrap_err()
            .to_string()
            .contains("nothing written"));
        assert_eq!(run.file.as_deref(), Some(broken));

        let run = run_script(
            script(&[&[Yes], &basics(), &[No, No, Yes]]),
            Some(broken),
            false,
        );
        run.result.unwrap();
        assert!(run.file.unwrap().contains("enabled = [\"unifi\"]"));
        assert_eq!(
            std::fs::read_to_string(backup_path(&run.path)).unwrap(),
            broken
        );
    }

    #[test]
    fn an_answer_that_is_not_a_variable_name_is_asked_again() {
        let mut answers = basics();
        answers[4] = Yes; // unifi needs credentials
        answers.splice(
            5..5,
            [
                Text("token"),
                Text("not a name!"),
                Text("env:UNIFI_TOKEN"),
                No, // another
            ],
        );
        let run = run_script(script(&[&answers, &[No, No, Yes]]), None, false);
        run.result.unwrap();
        assert!(
            run.transcript.contains("isn't a variable name"),
            "{}",
            run.transcript
        );
        assert!(run.file.unwrap().contains("token = \"env:UNIFI_TOKEN\""));
    }

    #[test]
    fn a_bad_socks_address_is_asked_again() {
        let mut answers = basics();
        answers[5] = Pick("socks5");
        answers.splice(6..6, [Text("localhost"), Text("127.0.0.1:1080")]);
        let run = run_script(script(&[&answers, &[No, No, Yes]]), None, false);
        run.result.unwrap();
        assert!(run
            .file
            .unwrap()
            .contains("default = { socks5 = { addr = \"127.0.0.1:1080\" } }"));
    }

    #[cfg(feature = "subscriber-email")]
    #[test]
    fn a_bad_email_address_is_caught_before_anything_is_written() {
        let run = run_script(
            script(&[
                &basics(),
                &[
                    No,
                    Yes,
                    Pick("Resend"),
                    Default,
                    Text("not an address"),
                    Text("ops@example.test"),
                    Default,
                    Default,
                    No, // don't go through again
                ],
            ]),
            None,
            false,
        );
        let err = run.result.unwrap_err().to_string();
        assert!(err.contains("nothing written"), "{err}");
        assert!(
            run.transcript.contains("doesn't work"),
            "{}",
            run.transcript
        );
        assert!(run.file.is_none());
    }

    /// A finished survey that wrote `rendered` and asked for both tests.
    #[cfg(any(feature = "subscriber-email", feature = "subscriber-webhook"))]
    fn outcome_with_tests(rendered: String) -> Outcome {
        Outcome {
            rendered,
            test_email: true,
            test_webhook: true,
        }
    }

    #[cfg(feature = "subscriber-email")]
    #[tokio::test]
    async fn the_test_email_goes_through_the_real_subscriber() {
        let (addr, received) = crate::commands::test_support::fake_smtp_server();
        let rendered = format!(
            "[subscribers.email]\nhost = \"127.0.0.1\"\nport = {}\ntls = \"none\"\n\
             from = \"delve@example.test\"\nto = [\"ops@example.test\"]\n",
            addr.port()
        );
        send_tests(&outcome_with_tests(rendered)).await;
        let log = received.lock().unwrap().join("\n");
        assert!(log.contains("RCPT TO:<ops@example.test>"), "{log}");
        assert!(log.contains("delve survey test message"), "{log}");
    }

    #[cfg(feature = "subscriber-webhook")]
    #[tokio::test]
    async fn the_test_webhook_goes_through_the_real_subscriber() {
        use std::io::{BufRead, BufReader, Read};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut length, mut line) = (0, String::new());
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                let header = line.trim_end().to_ascii_lowercase();
                if header.is_empty() {
                    break;
                }
                if let Some(v) = header.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
            String::from_utf8(body).unwrap()
        });
        send_tests(&outcome_with_tests(format!(
            "[subscribers.webhook]\nurl = \"{url}\"\n"
        )))
        .await;
        let body: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(body["schema"], 1);
        assert_eq!(
            body["events"][0]["display_name"],
            "delve survey test message"
        );
    }

    #[test]
    fn env_names_and_secrets_are_told_apart() {
        assert!(is_env_name("RESEND_API_KEY"));
        assert!(is_env_name("_x1"));
        assert!(!is_env_name("1ABC"));
        assert!(!is_env_name("a-b"));
        assert!(!is_env_name(""));
        assert!(looks_like_a_secret("re_9xYzAbCdEf12345678"));
        assert!(!looks_like_a_secret("POSTMARK_SERVER_TOKEN"));
        assert!(!looks_like_a_secret("SES_SMTP_PASSWORD2"));
    }

    #[test]
    fn the_backup_sits_next_to_the_file() {
        assert_eq!(
            backup_path(Path::new("/x/delve/config.toml")),
            Path::new("/x/delve/config.toml.bak")
        );
    }
}
