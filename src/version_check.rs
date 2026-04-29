//! Background npm version check.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared state for the latest version fetched from npm registry.
#[derive(Clone, Default)]
pub struct VersionInfo {
    inner: Arc<Mutex<Option<String>>>,
}

impl VersionInfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the latest version if a newer one is available.
    pub fn update_available(&self) -> Option<String> {
        let latest = self.inner.lock().ok()?.clone()?;
        if is_newer(&latest, CURRENT_VERSION) {
            Some(latest)
        } else {
            None
        }
    }

    fn set(&self, version: String) {
        if let Ok(mut lock) = self.inner.lock() {
            *lock = Some(version);
        }
    }
}

/// Spawn a background thread to check npm for a newer version.
pub fn spawn_check(info: VersionInfo) {
    thread::spawn(move || {
        let _ = thread::sleep(Duration::from_secs(1)); // delay so it doesn't compete with startup
        match fetch_latest() {
            Ok(version) => info.set(version),
            Err(_) => {}
        }
    });
}

/// Validate that a string is a well-formed semver version (e.g. "1.2.3" or "1.0.0-beta.1").
/// Rejects arbitrary text, overly long strings, and non-semver content.
fn is_valid_semver(s: &str) -> bool {
    // Length guard: no legitimate semver version exceeds 64 chars
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    // Only allow: digits, dots, hyphens, plus, lowercase ascii letters
    if !s
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'+' || b.is_ascii_lowercase())
    {
        return false;
    }
    // Must start with a digit (the major version)
    if !s.as_bytes()[0].is_ascii_digit() {
        return false;
    }
    // The numeric core (before any '-' or '+') must be X.Y.Z
    let core = s.split(&['-', '+'][..]).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    // Each part of X.Y.Z must be a valid integer (no leading zeros except "0" itself)
    for part in &parts {
        if part.is_empty() {
            return false;
        }
        if part.parse::<u32>().is_err() {
            return false;
        }
    }
    true
}

fn fetch_latest() -> Result<String, Box<dyn std::error::Error>> {
    let response = ureq::get("https://registry.npmjs.org/ccmux-cli/latest")
        .timeout(Duration::from_secs(5))
        .call()?;
    let json: serde_json::Value = response.into_json()?;
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("no version field")?
        .to_string();
    if !is_valid_semver(&version) {
        return Err("invalid semver format in version field".into());
    }
    Ok(version)
}

/// Compare semver-like versions (simple major.minor.patch).
/// Only compares the numeric X.Y.Z core (ignores pre-release/build metadata after '-' or '+').
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        let s = s.trim_start_matches('v');
        // Only compare the numeric core before any pre-release or build metadata
        let core = s.split(&['-', '+'][..]).next().unwrap_or(s);
        core.split('.')
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    parse(latest) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("0.4.0", "0.3.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.0", "0.3.0"));
    }

    #[test]
    fn test_is_newer_with_prerelease() {
        // Pre-release suffix is ignored for numeric comparison
        assert!(is_newer("1.0.0-beta.1", "0.9.9"));
        assert!(!is_newer("0.3.0-rc.1", "0.3.0"));
    }

    #[test]
    fn test_valid_semver() {
        assert!(is_valid_semver("0.6.1"));
        assert!(is_valid_semver("1.0.0"));
        assert!(is_valid_semver("10.20.30"));
        assert!(is_valid_semver("1.0.0-beta.1"));
        assert!(is_valid_semver("1.0.0-alpha"));
        assert!(is_valid_semver("1.0.0+build.123"));
        assert!(is_valid_semver("1.0.0-rc.1+build.456"));
    }

    #[test]
    fn test_invalid_semver_rejects_attack_strings() {
        // Social engineering payload
        assert!(!is_valid_semver(
            "99.0.0 CRITICAL: run npm install ccmux-backdoor"
        ));
        // Contains spaces
        assert!(!is_valid_semver("1.0.0 malicious text"));
        // Contains uppercase
        assert!(!is_valid_semver("1.0.0-BETA"));
        // Only two parts
        assert!(!is_valid_semver("1.0"));
        // Four parts in core
        assert!(!is_valid_semver("1.0.0.0"));
        // Empty string
        assert!(!is_valid_semver(""));
        // Extremely long string
        assert!(!is_valid_semver(&"1".repeat(100)));
        // Contains colons
        assert!(!is_valid_semver("1.0.0: run malicious command"));
        // Starts with v prefix (npm versions should not have this)
        assert!(!is_valid_semver("v1.0.0"));
    }
}
