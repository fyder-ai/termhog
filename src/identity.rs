//! Persistent anonymous person id, mirroring posthog-js's device id: a UUIDv7
//! generated once and reused, so the same user maps to one PostHog person across
//! runs. An explicit id (config/env) acts like `identify()`.

use std::fs;
use std::path::PathBuf;

use uuid::Uuid;

/// Resolve the distinct_id: an explicit override wins; otherwise reuse the id
/// persisted in the config dir, generating and storing one on first use.
pub fn resolve(override_id: Option<String>) -> String {
    if let Some(id) = override_id.filter(|s| !s.is_empty()) {
        return id;
    }
    persisted().unwrap_or_else(generate_and_store)
}

/// `$XDG_CONFIG_HOME/ph-capture/distinct_id`, falling back to `~/.config/...`.
fn config_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(dir.join("ph-capture").join("distinct_id"))
}

fn persisted() -> Option<String> {
    let id = fs::read_to_string(config_path()?).ok()?.trim().to_string();
    (!id.is_empty()).then_some(id)
}

fn generate_and_store() -> String {
    let id = Uuid::now_v7().to_string();
    if let Some(path) = config_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, &id);
    }
    id
}
