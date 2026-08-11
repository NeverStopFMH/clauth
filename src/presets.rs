//! Named endpoint + model templates a Setup-tab account can be stamped from.
//!
//! A preset carries exactly the two things that make an account talk to a given
//! provider: its `base_url` and its [`ModelSettings`]. Credentials, env, and
//! every fallback knob stay out — those are per-account, and a template that
//! carried them would silently move an api key between accounts.
//!
//! Two built-ins ship in the binary; the rest live one JSON file per preset
//! under `~/.clauth/presets/`. The file NAME is the preset name, so it goes
//! through [`crate::actions::validate_profile_name`] (the same charset that
//! bounds a profile directory) before it ever reaches a path — see
//! `docs/security.md`.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::profile::{ModelSettings, atomic_write_600, clauth_dir, read_json_file};

/// A named `base_url` + [`ModelSettings`] template.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Preset {
    pub(crate) name: String,
    pub(crate) base_url: Option<String>,
    pub(crate) models: ModelSettings,
    /// Ships in the binary: never written, never deleted, never overwritten.
    pub(crate) builtin: bool,
}

/// On-disk shape of `~/.clauth/presets/<name>.json`. The name is the file stem,
/// so it is deliberately absent from the body — one spelling, no way for the two
/// to disagree.
#[derive(Debug, Serialize, Deserialize)]
struct PresetFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(default)]
    models: ModelSettings,
}

/// The built-ins, in menu order. Each sets only `models.default` — CC's
/// top-level `model` setting, the fallback every alias resolves through when no
/// per-tier override covers it — so applying one leaves the tier rows free for
/// the operator to pin afterwards.
const BUILTINS: [(&str, &str, &str); 2] = [
    (
        "DeepSeek",
        "https://api.deepseek.com/anthropic",
        "deepseek-chat",
    ),
    ("Z.ai", "https://api.z.ai/api/anthropic", "glm-5.2"),
];

fn builtins() -> Vec<Preset> {
    BUILTINS
        .iter()
        .map(|(name, base_url, model)| Preset {
            name: (*name).to_string(),
            base_url: Some((*base_url).to_string()),
            models: ModelSettings {
                default: Some((*model).to_string()),
                ..ModelSettings::default()
            },
            builtin: true,
        })
        .collect()
}

/// Whether `name` collides with a built-in. Case-insensitive: a
/// case-folding filesystem would let `deepseek.json` shadow the built-in's slot
/// on one host and not another, so the refusal can't depend on spelling.
pub(crate) fn is_builtin(name: &str) -> bool {
    BUILTINS
        .iter()
        .any(|(builtin, _, _)| builtin.eq_ignore_ascii_case(name.trim()))
}

fn presets_dir() -> Result<std::path::PathBuf> {
    Ok(clauth_dir()?.join("presets"))
}

/// The name's own path, refusing anything that isn't a bare filename. Reuses the
/// profile-name charset (`[A-Za-z0-9-_.@+]`, no leading `.`), which is what keeps
/// a separator or a `..` out of the join.
fn preset_path(name: &str) -> Result<std::path::PathBuf> {
    let trimmed = name.trim();
    crate::actions::validate_profile_name(trimmed, &[], None)?;
    Ok(presets_dir()?.join(format!("{trimmed}.json")))
}

/// Built-ins first, then the on-disk ones sorted by name. A file that won't
/// parse is skipped rather than failing the whole list — one hand-edited preset
/// must not hide the others from the picker.
pub(crate) fn list_presets() -> Vec<Preset> {
    let mut out = builtins();
    let Ok(dir) = presets_dir() else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut custom: Vec<Preset> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // A built-in's slot is never readable from disk: the binary's copy is
        // the only definition, so a stray file with that stem stays invisible
        // instead of quietly shadowing it.
        if is_builtin(name) {
            continue;
        }
        let Ok(file) = read_json_file::<PresetFile>(&path) else {
            continue;
        };
        custom.push(Preset {
            name: name.to_string(),
            base_url: file.base_url,
            models: file.models,
            builtin: false,
        });
    }
    custom.sort_by_key(|a| a.name.to_lowercase());
    out.extend(custom);
    out
}

/// Built-ins first, then disk. `None` when neither carries the name.
pub(crate) fn load_preset(name: &str) -> Option<Preset> {
    let trimmed = name.trim();
    if let Some(p) = builtins()
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(trimmed))
    {
        return Some(p);
    }
    let path = preset_path(trimmed).ok()?;
    let file = read_json_file::<PresetFile>(&path).ok()?;
    Some(Preset {
        name: trimmed.to_string(),
        base_url: file.base_url,
        models: file.models,
        builtin: false,
    })
}

/// Whether a custom preset already occupies `name`. Callers confirm before
/// [`save_preset`] overwrites it.
pub(crate) fn preset_exists(name: &str) -> bool {
    preset_path(name).is_ok_and(|p| p.exists())
}

/// Write `name` to disk, replacing any custom preset already there. Refuses a
/// built-in name outright — the binary's copy is the definition, so a file in
/// that slot would be written and then never read.
pub(crate) fn save_preset(
    name: &str,
    base_url: &Option<String>,
    models: &ModelSettings,
) -> Result<()> {
    let trimmed = name.trim();
    if is_builtin(trimmed) {
        bail!("'{trimmed}' is a built-in preset and cannot be overwritten");
    }
    let path = preset_path(trimmed)?;
    let body = serde_json::to_string_pretty(&PresetFile {
        base_url: base_url.clone(),
        models: models.clone(),
    })?;
    // `atomic_write_600` creates a missing parent 0o700 itself, so the dir and
    // the file are both born owner-only (`docs/security.md`).
    atomic_write_600(&path, format!("{body}\n"))?;
    Ok(())
}

pub(crate) fn delete_preset(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if is_builtin(trimmed) {
        bail!("'{trimmed}' is a built-in preset and cannot be deleted");
    }
    let path = preset_path(trimmed)?;
    if !path.exists() {
        bail!("no preset named '{trimmed}'");
    }
    std::fs::remove_file(&path)?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/presets.rs"]
mod tests;
