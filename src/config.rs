// ─── User configuration (config.toml) ─────────────────────
//
// 起動時に 1 回だけ読み込むユーザー設定。現状の項目は拡張子別の
// 外部オープナー([[openers]])のみ。設定ファイルが無い・壊れている
// 場合は空設定で継続し(fail-open)、従来挙動を完全に維持する。
//
// 置き場所: <dirs::config_dir()>/ccmux/config.toml
//   Windows: %APPDATA%\ccmux\config.toml
//   Linux:   ~/.config/ccmux/config.toml
//   macOS:   ~/Library/Application Support/ccmux/config.toml
//
// 例:
//   [[openers]]
//   extensions = ["md", "markdown"]
//   command = "typora"            # Windows の .cmd シムは "code.cmd" のように拡張子まで書く
//   args = ["{file}"]             # {file} = 絶対パス。省略時は末尾に自動付加

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 拡張子と外部コマンドの対応 1 件(config.toml の [[openers]])。
#[derive(Debug, Clone, Deserialize)]
pub struct Opener {
    /// 対象拡張子(ドットなし)。比較は小文字化して行う。
    /// 複合拡張子は不可: "tar.gz" ではなく "gz" と書く
    /// (判定は Path::extension() = 最後のドット以降のみ)。
    pub extensions: Vec<String>,
    /// 起動コマンド。PATH 解決は OS に任せる(シェルは経由しない)。
    pub command: String,
    /// 引数。"{file}" は対象ファイルの絶対パスに置換され、
    /// どの引数にも現れない場合は末尾に自動付加される。
    #[serde(default)]
    pub args: Vec<String>,
}

/// ccmux のユーザー設定。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub openers: Vec<Opener>,
}

impl Config {
    /// 設定ファイルを読み込む。戻り値の .1 は起動を止めない警告。
    /// ファイルが存在しない(読めない)場合は空設定・警告なし。
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = config_path() else {
            return (Self::default(), None);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (
                    Self::default(),
                    Some(format!("config.toml 読み込み失敗(設定を無視して継続): {}", e)),
                ),
            },
            Err(_) => (Self::default(), None),
        }
    }

    /// TOML テキストをパースする(load から分離してテスト可能に)。
    /// Windows のエディタが付ける UTF-8 BOM は除去してから渡す。
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        toml::from_str::<Self>(text).map_err(|e| {
            // toml のエラーは複数行になるためステータスバー向けに先頭行だけ残す
            e.to_string().lines().next().unwrap_or("parse error").to_string()
        })
    }

    /// path の拡張子に対応するオープナーを返す(小文字化して比較)。
    pub fn find_opener(&self, path: &Path) -> Option<&Opener> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        self.openers
            .iter()
            .find(|o| o.extensions.iter().any(|e| e.to_lowercase() == ext))
    }
}

/// オープナーの args を実引数へ展開する。"{file}" を abs_path に置換し、
/// どの引数にも現れない場合は末尾に付加する。
pub fn build_args(opener: &Opener, abs_path: &str) -> Vec<String> {
    let mut replaced = false;
    let mut out: Vec<String> = opener
        .args
        .iter()
        .map(|a| {
            if a.contains("{file}") {
                replaced = true;
                a.replace("{file}", abs_path)
            } else {
                a.clone()
            }
        })
        .collect();
    if !replaced {
        out.push(abs_path.to_string());
    }
    out
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ccmux").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opener(exts: &[&str], args: &[&str]) -> Opener {
        Opener {
            extensions: exts.iter().map(|s| s.to_string()).collect(),
            command: "dummy".to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_parse_valid_toml() {
        let cfg = Config::parse(
            r#"
            [[openers]]
            extensions = ["md", "markdown"]
            command = "typora"
            args = ["{file}"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.openers.len(), 1);
        assert_eq!(cfg.openers[0].command, "typora");
        assert_eq!(cfg.openers[0].extensions, vec!["md", "markdown"]);
    }

    #[test]
    fn test_parse_invalid_toml_returns_err() {
        // 型不一致(extensions が文字列)はエラーになる
        let err = Config::parse("[[openers]]\nextensions = \"md\"\ncommand = \"x\"").unwrap_err();
        assert!(!err.is_empty());
        // エラーは 1 行に収まっている(ステータスバー表示用)
        assert!(!err.contains('\n'));
    }

    #[test]
    fn test_parse_empty_is_default() {
        let cfg = Config::parse("").unwrap();
        assert!(cfg.openers.is_empty());
    }

    #[test]
    fn test_parse_strips_utf8_bom() {
        let cfg = Config::parse("\u{feff}[[openers]]\nextensions = [\"md\"]\ncommand = \"x\"")
            .unwrap();
        assert_eq!(cfg.openers.len(), 1);
    }

    #[test]
    fn test_find_opener_case_insensitive() {
        let cfg = Config { openers: vec![opener(&["md"], &[])] };
        assert!(cfg.find_opener(Path::new("README.MD")).is_some());
        assert!(cfg.find_opener(Path::new("note.Md")).is_some());
    }

    #[test]
    fn test_find_opener_unregistered_or_no_ext() {
        let cfg = Config { openers: vec![opener(&["md"], &[])] };
        assert!(cfg.find_opener(Path::new("main.rs")).is_none());
        assert!(cfg.find_opener(Path::new("Makefile")).is_none());
    }

    #[test]
    fn test_build_args_replaces_placeholder() {
        let o = opener(&["md"], &["--open", "{file}"]);
        assert_eq!(
            build_args(&o, "C:/work/a.md"),
            vec!["--open".to_string(), "C:/work/a.md".to_string()]
        );
    }

    #[test]
    fn test_build_args_appends_when_missing() {
        let o = opener(&["md"], &["--new-window"]);
        assert_eq!(
            build_args(&o, "/tmp/a.md"),
            vec!["--new-window".to_string(), "/tmp/a.md".to_string()]
        );
        let empty = opener(&["md"], &[]);
        assert_eq!(build_args(&empty, "/tmp/a.md"), vec!["/tmp/a.md".to_string()]);
    }
}
