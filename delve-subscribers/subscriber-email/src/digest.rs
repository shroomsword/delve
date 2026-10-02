//! The one message that stands for all the events of a dig. A single event
//! still gets the ordinary message (`render` in `lib.rs`); this is for two or
//! more.

use std::collections::BTreeSet;

use delve_core::prelude::*;

use crate::{device_label, single_line};

/// Orders events so a digest is stable and readable: grouped by device
/// (vendor, family, hardware), newest version first within a device.
pub(crate) fn sorted(mut events: Vec<FirmwareEvent>) -> Vec<FirmwareEvent> {
    events.sort_by(|a, b| {
        let (fa, fb) = (firmware(a), firmware(b));
        device_key(fa)
            .cmp(&device_key(fb))
            .then_with(|| fb.version.ordinal.cmp(&fa.version.ordinal))
            .then_with(|| fb.version.raw.cmp(&fa.version.raw))
    });
    events
}

fn firmware(event: &FirmwareEvent) -> &FirmwareMetadata {
    match event {
        FirmwareEvent::NewRelease { firmware, .. }
        | FirmwareEvent::UpdatedRelease { firmware, .. } => firmware,
    }
}

fn device_key(firmware: &FirmwareMetadata) -> (String, String, String) {
    (
        firmware.vendor.clone(),
        firmware.device_family.clone(),
        firmware.hardware_targets.join("+"),
    )
}

/// The subject and body for `events`, which must already be [`sorted`].
/// `part` is `(this, of)` when a burst is split across several messages.
pub(crate) fn render_digest(
    events: &[FirmwareEvent],
    part: Option<(usize, usize)>,
) -> (String, String) {
    let count = events.len();
    let noun = if count == 1 { "change" } else { "changes" };
    let devices: BTreeSet<_> = events.iter().map(|e| device_key(firmware(e))).collect();
    let vendors: BTreeSet<_> = devices.iter().map(|d| d.0.as_str()).collect();

    let what = match (devices.len(), vendors.len()) {
        (1, _) => device_label(firmware(&events[0])),
        (n, 1) => format!("{} ({n} devices)", vendors.iter().next().unwrap_or(&"")),
        (n, _) => format!("{n} devices"),
    };
    let mut subject = format!("[delve] {count} firmware {noun}: {what}");
    if let Some((this, of)) = part {
        subject.push_str(&format!(" (part {this} of {of})"));
    }

    let mut body = format!("{count} firmware {noun} found in this dig");
    if let Some((this, of)) = part {
        body.push_str(&format!(
            " (part {this} of {of}; the rest are in other messages)"
        ));
    }
    body.push_str(".\n");

    let mut current: Option<(String, String, String)> = None;
    for event in events {
        let key = device_key(firmware(event));
        if current.as_ref() != Some(&key) {
            let fw = firmware(event);
            body.push('\n');
            body.push_str(&device_label(fw));
            if let Some(name) = &fw.display_name {
                body.push_str(&format!(" - {name}"));
            }
            body.push('\n');
            current = Some(key);
        }
        body.push_str(&entry(event));
    }
    (single_line(&subject), body)
}

/// One event as a few indented lines under its device's heading.
fn entry(event: &FirmwareEvent) -> String {
    let fw = firmware(event);
    let (label, versions, old_sha) = match event {
        FirmwareEvent::NewRelease { .. } => ("new", fw.version.raw.clone(), None),
        FirmwareEvent::UpdatedRelease {
            previous,
            changed_fields,
            version_direction,
            ..
        } => {
            let old_sha = changed_fields
                .iter()
                .find(|d| d.field == "sha256")
                .map(|d| short(&d.before));
            if previous.version.raw == fw.version.raw {
                ("rebuilt", fw.version.raw.clone(), old_sha)
            } else {
                let label = match version_direction {
                    VersionDirection::Newer => "updated",
                    VersionDirection::Older => "DOWNGRADED",
                    VersionDirection::Unordered => "changed",
                };
                (
                    label,
                    format!("{} -> {}", previous.version.raw, fw.version.raw),
                    old_sha,
                )
            }
        }
    };

    let indent = " ".repeat(13);
    let mut out = format!("  {label:<10} {versions}\n");

    let mut details = Vec::new();
    if let Some(date) = fw.release_date {
        details.push(format!("released {date}"));
    }
    if let Some(sha) = fw.sha256 {
        let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
        details.push(match old_sha {
            Some(old) => format!("sha256 {} (was {old})", short(&hex)),
            None => format!("sha256 {}", short(&hex)),
        });
    }
    if !details.is_empty() {
        out.push_str(&format!("{indent}{}\n", details.join(", ")));
    }
    if let Some(url) = &fw.release_notes_url {
        out.push_str(&format!("{indent}notes {url}\n"));
    }
    out.push_str(&format!("{indent}{}\n", fw.source_url));
    out
}

/// The first 12 hex digits of a hash, enough to tell two apart.
fn short(hex: &str) -> String {
    hex.chars().take(12).collect()
}
