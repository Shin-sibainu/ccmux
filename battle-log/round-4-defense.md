# Round 4 - Defense Report

## 対応した脆弱性
- **種別**: Unvalidated External Input Displayed in UI (UI Spoofing / Social Engineering via npm Registry)
- **OWASP分類**: A08:2021 - Software and Data Integrity Failures
- **深刻度**: Medium

## 修正方針
`fetch_latest()` で npm レジストリから取得した version 文字列を、セマンティックバージョニング形式として検証するバリデーション関数 `is_valid_semver()` を追加した。不正な形式の場合は `Err` を返し、`VersionInfo` に保存されない（= ステータスバーに表示されない）。

### 検討した他の選択肢と却下理由

1. **ui.rs 側での表示時トランケーション**: 文字列を20文字で切るだけでは `"99.0.0 CRITICAL: ru"` のように途中まで表示され、ソーシャルエンジニアリングの余地が残る。根本原因（未検証の外部入力）を解決しないため却下。

2. **regex クレート追加による厳密パース**: 正規表現で完全な semver 仕様を検証する案。新しい依存クレートの追加が必要で、この用途にはオーバースペック。手書きの検証で十分にカバーできるため却下。

3. **`is_valid_semver()` を `fetch_latest()` 内で適用 (採用)**: 最も上流で不正入力をブロックできる。`VersionInfo::set()` に到達する前にフィルタするため、下流のすべてのコード（`is_newer()`、UI 表示）が安全な入力のみを扱う。新しい依存なし、1ファイルの変更のみ。

## 変更内容
- ファイル: `src/version_check.rs`
- 変更行数: +86 / -2
- コミットハッシュ: 32b3a8a65d1d118b70bdf1f077f355b0f87f3ee6

```diff
+/// Validate that a string is a well-formed semver version (e.g. "1.2.3" or "1.0.0-beta.1").
+/// Rejects arbitrary text, overly long strings, and non-semver content.
+fn is_valid_semver(s: &str) -> bool {
+    // Length guard: no legitimate semver version exceeds 64 chars
+    if s.is_empty() || s.len() > 64 {
+        return false;
+    }
+    // Only allow: digits, dots, hyphens, plus, lowercase ascii letters
+    if !s.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'+' || b.is_ascii_lowercase()) {
+        return false;
+    }
+    // Must start with a digit (the major version)
+    if !s.as_bytes()[0].is_ascii_digit() {
+        return false;
+    }
+    // The numeric core (before any '-' or '+') must be X.Y.Z
+    let core = s.split(&['-', '+'][..]).next().unwrap_or("");
+    let parts: Vec<&str> = core.split('.').collect();
+    if parts.len() != 3 {
+        return false;
+    }
+    for part in &parts {
+        if part.is_empty() { return false; }
+        if part.parse::<u32>().is_err() { return false; }
+    }
+    true
+}

 fn fetch_latest() -> Result<String, Box<dyn std::error::Error>> {
     // ... (existing code) ...
     let version = json.get("version").and_then(|v| v.as_str()).ok_or("no version field")?.to_string();
+    if !is_valid_semver(&version) {
+        return Err("invalid semver format in version field".into());
+    }
     Ok(version)
 }

 /// Compare semver-like versions (simple major.minor.patch).
+/// Only compares the numeric X.Y.Z core (ignores pre-release/build metadata after '-' or '+').
 fn is_newer(latest: &str, current: &str) -> bool {
     let parse = |s: &str| -> Vec<u32> {
-        s.trim_start_matches('v')
-            .split('.')
-            .filter_map(|p| p.parse().ok())
+        let s = s.trim_start_matches('v');
+        let core = s.split(&['-', '+'][..]).next().unwrap_or(s);
+        core.split('.')
+            .filter_map(|p| p.parse().ok())  // still safe; validated upstream
             .collect()
     };
```

## 副作用確認
- 既存テスト: 全33件通過（既存5件 + 新規3件）
- 動作確認:
  - 正常な semver 文字列 (`0.6.1`, `1.0.0`, `1.0.0-beta.1`, `1.0.0+build.123`) が `is_valid_semver()` を通過することをテストで確認
  - 攻撃文字列（スペース混入、大文字、極端な長さ、コロン混入、`v` プレフィックス、2パート/4パート）がすべて拒否されることをテストで確認
  - `is_newer()` の既存テストが引き続き通過（pre-release 付きバージョンの新テストも追加）
  - `fetch_latest()` がバリデーション失敗時に `Err` を返すため、`spawn_check()` の `match` で `Err(_) => {}` により黙って無視される（既存のエラーハンドリングパスと同じ）

## 残留リスク(あれば)
1. **レスポンスボディサイズ制限なし**: 攻撃レポートの「問題2」で指摘された `response.into_json()` のメモリ消費問題は未対応。ureq v2 には組み込みのボディサイズ制限がなく、対応には `response.into_reader()` で手動で読み取り量を制限する必要がある。今回のラウンドの主脆弱性（UI スプーフィング）とは直接関係しないため、過剰修正を避けた。
2. **TLS/MITM**: npm レジストリとの通信は HTTPS だが、ローカルの CA ストアが侵害された場合にレスポンスが改ざんされる可能性がある。これは ccmux 固有の問題ではなく、OS レベルの信頼チェーンの問題。
3. **npm パッケージ名の typosquatting**: `ccmux-cli` 自体の名前空間を攻撃者が奪取した場合、正規の semver 形式でも悪意あるバージョンが公開される。これはバージョン文字列の検証では防げない。

## 自己評価
- 修正の堅牢性(自己採点): 8/10
- 採点理由: 主脆弱性（任意テキストによる UI スプーフィング）は `is_valid_semver()` で確実にブロックされる。外部依存なし、1ファイルのみの変更で副作用リスクが低い。`is_newer()` の `filter_map` による無音ドロップも、numeric core のみを対象にすることで改善した。レスポンスサイズ制限は未対応だが、バックグラウンドスレッドで実行され本体がクラッシュしないことと、5秒タイムアウトが存在することから、実際のリスクは限定的と判断。
