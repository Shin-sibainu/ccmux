//! Multi-pane layout configuration loaded from TOML.
//!
//! A layout is a binary tree of panes, where each leaf is a pane (with an
//! optional command) and each interior node is a split with a direction
//! (`vertical` or `horizontal`) and a ratio.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Deserialize, PartialEq)]
pub struct LayoutConfig {
    pub version: u8,
    pub name: String,
    pub root: LayoutNodeSpec,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutNodeSpec {
    Pane {
        id: String,
        #[serde(default)]
        command: Option<String>,
    },
    Split {
        direction: DirectionSpec,
        ratio: f32,
        first: Box<LayoutNodeSpec>,
        second: Box<LayoutNodeSpec>,
    },
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum DirectionSpec {
    Vertical,
    Horizontal,
}

const SUPPORTED_VERSION: u8 = 1;
const MAX_PANES: usize = 16;
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;
const ID_PATTERN_HINT: &str = "id must contain only [A-Za-z0-9_-]";

impl LayoutConfig {
    /// Parse a layout from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(s).context("failed to parse layout TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Resolve the path to a layout file by name. Search order:
    ///   1. `$CCMUX_LAYOUTS_DIR/<name>.toml` (env override, for tests)
    ///   2. `./ccmux-layouts/<name>.toml` (project local)
    ///   3. `~/.config/ccmux/layouts/<name>.toml` (user global)
    pub fn resolve_path(name: &str) -> Result<PathBuf> {
        let candidates = Self::candidate_paths(name);
        for p in &candidates {
            if p.is_file() {
                return Ok(p.clone());
            }
        }
        let listed = candidates
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        Err(anyhow!("layout '{name}' not found. Searched:\n{listed}"))
    }

    fn candidate_paths(name: &str) -> Vec<PathBuf> {
        let env_dir = std::env::var("CCMUX_LAYOUTS_DIR").ok();
        candidate_paths_from(name, env_dir.as_deref(), dirs::config_dir())
    }

    /// Load and parse a layout by name.
    pub fn load(name: &str) -> Result<Self> {
        let path = Self::resolve_path(name)?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read layout file {}", path.display()))?;
        Self::from_toml_str(&content)
    }

    /// Validate the parsed layout: schema version, ratios, id uniqueness,
    /// id character set, and pane count.
    pub fn validate(&self) -> Result<()> {
        if self.version != SUPPORTED_VERSION {
            return Err(anyhow!(
                "unsupported layout version {} (this build expects {})",
                self.version,
                SUPPORTED_VERSION
            ));
        }
        if self.name.trim().is_empty() {
            return Err(anyhow!("layout name must not be empty"));
        }
        let mut ids = HashSet::new();
        let mut count = 0usize;
        validate_node(&self.root, &mut ids, &mut count)?;
        if count == 0 {
            return Err(anyhow!("layout must contain at least one pane"));
        }
        if count > MAX_PANES {
            return Err(anyhow!("layout has {count} panes (max {MAX_PANES})",));
        }
        Ok(())
    }
}

/// Pure version of `candidate_paths` that takes the env override and
/// user-config dir as arguments. Split out so tests can verify the
/// search order without mutating the process-wide `CCMUX_LAYOUTS_DIR`
/// env var (which races under `cargo test`'s parallel runner).
fn candidate_paths_from(
    name: &str,
    env_dir: Option<&str>,
    user_cfg_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let file = format!("{name}.toml");
    let mut out = Vec::new();
    if let Some(env_dir) = env_dir {
        if !env_dir.is_empty() {
            out.push(PathBuf::from(env_dir).join(&file));
        }
    }
    out.push(PathBuf::from("ccmux-layouts").join(&file));
    if let Some(cfg_dir) = user_cfg_dir {
        out.push(cfg_dir.join("ccmux").join("layouts").join(&file));
    }
    out
}

fn validate_node(
    node: &LayoutNodeSpec,
    ids: &mut HashSet<String>,
    count: &mut usize,
) -> Result<()> {
    match node {
        LayoutNodeSpec::Pane { id, .. } => {
            if id.trim().is_empty() {
                return Err(anyhow!("pane id must not be empty"));
            }
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(anyhow!("invalid pane id '{id}': {ID_PATTERN_HINT}"));
            }
            if !ids.insert(id.clone()) {
                return Err(anyhow!("duplicate pane id '{id}'"));
            }
            *count += 1;
            Ok(())
        }
        LayoutNodeSpec::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !ratio.is_finite() {
                return Err(anyhow!("split ratio must be a finite number"));
            }
            if *ratio < MIN_RATIO || *ratio > MAX_RATIO {
                return Err(anyhow!(
                    "split ratio {ratio} out of range ({MIN_RATIO}..={MAX_RATIO})",
                ));
            }
            validate_node(first, ids, count)?;
            validate_node(second, ids, count)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_pane_toml() -> &'static str {
        r#"
            version = 1
            name = "single"
            [root]
            type = "pane"
            id = "secretary"
            command = "claude /company"
        "#
    }

    fn nested_layout_toml() -> &'static str {
        r#"
            version = 1
            name = "cc-campany"
            [root]
            type = "split"
            direction = "vertical"
            ratio = 0.5

              [root.first]
              type = "pane"
              id = "secretary"
              command = "claude /company"

              [root.second]
              type = "split"
              direction = "horizontal"
              ratio = 0.5

                [root.second.first]
                type = "pane"
                id = "engineering"
                command = "cce"

                [root.second.second]
                type = "pane"
                id = "research"
                command = "ccr"
        "#
    }

    #[test]
    fn parses_single_pane_layout() {
        let cfg = LayoutConfig::from_toml_str(single_pane_toml()).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.name, "single");
        match &cfg.root {
            LayoutNodeSpec::Pane { id, command } => {
                assert_eq!(id, "secretary");
                assert_eq!(command.as_deref(), Some("claude /company"));
            }
            _ => panic!("expected Pane at root"),
        }
    }

    #[test]
    fn parses_nested_split_layout() {
        let cfg = LayoutConfig::from_toml_str(nested_layout_toml()).unwrap();
        assert_eq!(cfg.name, "cc-campany");
    }

    #[test]
    fn rejects_unsupported_version() {
        let toml = r#"
            version = 99
            name = "x"
            [root]
            type = "pane"
            id = "a"
        "#;
        assert!(LayoutConfig::from_toml_str(toml).is_err());
    }

    #[test]
    fn rejects_empty_name() {
        let toml = r#"
            version = 1
            name = ""
            [root]
            type = "pane"
            id = "a"
        "#;
        assert!(LayoutConfig::from_toml_str(toml).is_err());
    }

    #[test]
    fn rejects_duplicate_pane_ids() {
        let toml = r#"
            version = 1
            name = "dup"
            [root]
            type = "split"
            direction = "vertical"
            ratio = 0.5
              [root.first]
              type = "pane"
              id = "a"
              [root.second]
              type = "pane"
              id = "a"
        "#;
        let err = LayoutConfig::from_toml_str(toml).unwrap_err().to_string();
        assert!(err.contains("duplicate pane id"), "{err}");
    }

    #[test]
    fn rejects_invalid_id_characters() {
        let toml = r#"
            version = 1
            name = "bad-id"
            [root]
            type = "pane"
            id = "has space"
        "#;
        assert!(LayoutConfig::from_toml_str(toml).is_err());
    }

    #[test]
    fn rejects_ratio_below_min() {
        let toml = r#"
            version = 1
            name = "x"
            [root]
            type = "split"
            direction = "vertical"
            ratio = 0.05
              [root.first]
              type = "pane"
              id = "a"
              [root.second]
              type = "pane"
              id = "b"
        "#;
        assert!(LayoutConfig::from_toml_str(toml).is_err());
    }

    #[test]
    fn rejects_ratio_above_max() {
        let toml = r#"
            version = 1
            name = "x"
            [root]
            type = "split"
            direction = "vertical"
            ratio = 0.95
              [root.first]
              type = "pane"
              id = "a"
              [root.second]
              type = "pane"
              id = "b"
        "#;
        assert!(LayoutConfig::from_toml_str(toml).is_err());
    }

    #[test]
    fn rejects_ratio_nan() {
        let toml = r#"
            version = 1
            name = "x"
            [root]
            type = "split"
            direction = "vertical"
            ratio = nan
              [root.first]
              type = "pane"
              id = "a"
              [root.second]
              type = "pane"
              id = "b"
        "#;
        assert!(LayoutConfig::from_toml_str(toml).is_err());
    }

    #[test]
    fn rejects_pane_without_id() {
        let toml = r#"
            version = 1
            name = "x"
            [root]
            type = "pane"
        "#;
        assert!(LayoutConfig::from_toml_str(toml).is_err());
    }

    #[test]
    fn env_override_comes_first() {
        // Test the pure helper directly so we don't race with other
        // tests on the shared `CCMUX_LAYOUTS_DIR` env var.
        let paths = candidate_paths_from(
            "foo",
            Some("/tmp/ccmux-test-layouts"),
            Some(PathBuf::from("/fake-user-cfg")),
        );
        assert_eq!(paths[0], PathBuf::from("/tmp/ccmux-test-layouts/foo.toml"));
    }

    #[test]
    fn empty_env_override_is_ignored() {
        // `CCMUX_LAYOUTS_DIR=""` must not become a bogus `./foo.toml`
        // candidate — we only honor a non-empty override.
        let paths = candidate_paths_from("foo", Some(""), Some(PathBuf::from("/fake-user-cfg")));
        assert_eq!(paths[0], PathBuf::from("ccmux-layouts/foo.toml"));
    }

    #[test]
    fn candidate_paths_fall_through_to_user_cfg() {
        // With no env override, first candidate is the project-local
        // directory and the user config dir is the fallback.
        let paths = candidate_paths_from("foo", None, Some(PathBuf::from("/home/user/.config")));
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("ccmux-layouts/foo.toml"));
        assert_eq!(
            paths[1],
            PathBuf::from("/home/user/.config/ccmux/layouts/foo.toml")
        );
    }
}
