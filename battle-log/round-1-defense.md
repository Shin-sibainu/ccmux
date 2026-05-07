# Round 1 - Defense Report

## 対応した脆弱性

- **種別**: Path Traversal via OSC 7 Escape Sequence (Unsanitized PTY Output Injection)
- **OWASP分類**: A03:2021 – Injection
- **深刻度**: High

PTY 出力から検出した OSC 7 シーケンスが `canonicalize()` + `is_dir()` のみで検証されており、  
初期 cwd との親子関係を確認していなかったため、pane 内のプログラムが任意の OSC 7 を出力するだけで  
FileTree ルートを任意ディレクトリ（`/etc`, `~/.ssh` 等）に変更できた。

---

## 修正方針

**採用: `initial_cwd` フィールドによる境界チェック（starts_with）**

`Workspace` 構造体に起動時の canonicalize 済み cwd を `initial_cwd` として保持し、  
`CwdChanged` ハンドラで `new_cwd.starts_with(&ws.initial_cwd)` が偽のパスは無視する。

この方針を選んだ理由:

1. **最小変更で確実**: 既存の `canonicalize()` チェックの直後に1行追加するだけで、  
   symlink・相対パス・`../` 全てを跨いだパスに対して有効に機能する。
2. **副作用が限定的**: `cd /other/project` のような意図的なワークスペース外移動は  
   FileTree に反映されなくなるが、それはこの機能の設計想定外の操作であり許容できる。  
   シェル自体はそのまま動作する（PTY への影響なし）。

**検討して却下した代替手段**:

- *OSC 7 の発信元プロセスを特定する*: PTY バイトストリームでは発信元を識別できず実装不可。
- *頻度ヒューリスティック*: 偽陰性・偽陽性ともに高く、実装コストに見合わない。
- *UI 確認ダイアログ*: UX が悪く、自動化スクリプトに対して防御にならない。

---

## 変更内容

- ファイル: `src/app.rs`
- 変更行数: +18 / -0
- コミットハッシュ: `9f4cce4dd7df0a60d5b8e5b5dcab76aec53117d1`

```diff
+    /// The canonicalized working directory at workspace creation time.
+    /// Used as the security boundary for OSC 7 CwdChanged events:
+    /// only paths that are sub-paths of this root are accepted.
+    pub initial_cwd: PathBuf,

 impl Workspace::new() {
+    // Canonicalize the initial cwd for use as the OSC 7 security boundary.
+    let initial_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
     Ok(Self {
         ...
+        initial_cwd,
         ...
     })
 }

 AppEvent::CwdChanged(pane_id, new_cwd) => {
     let new_cwd = match new_cwd.canonicalize() {
         Ok(p) if p.is_dir() => p,
         _ => continue,
     };
     for ws in &mut self.workspaces {
         if ws.panes.contains_key(&pane_id) {
+            // Security: reject OSC 7 paths that escape the workspace's
+            // initial working directory.
+            if !new_cwd.starts_with(&ws.initial_cwd) {
+                break;
+            }
             // Update pane's cwd ...
```

---

## 副作用確認

- **既存テスト**: `cargo test` — 30 tests passed, 0 failed
- **ビルド**: `cargo build` — Finished (no errors, no warnings)

正当なユースケース（`cd src/` 等のサブディレクトリ移動）は `starts_with` が真になるため  
引き続き FileTree に反映される。意図的な `cd /other/project` は反映されなくなるが、  
シェルは正常に動作し続ける（PTY への悪影響なし）。

---

## 残留リスク

1. **`cd /other/project` が FileTree に反映されない**: UX 上の制約として残る。  
   ユーザーが意図的に別プロジェクトに移動した場合、FileTree は古い初期 cwd を表示し続ける。  
   将来的には「ルート変更確認ダイアログ」の実装で解消できる。

2. **Windows MSYS2 パス変換**: `initial_cwd` の canonicalize は起動時 1 回のみのため、  
   MSYS2/Git Bash 環境でのパス変換の差異が境界チェックの誤検知につながる可能性がある。  
   ただし現在の `Workspace::new()` は `std::env::current_dir()` を利用しており、  
   同じ環境で canonicalize しているため実用上は問題ない。

3. **複数タブ間の独立性**: 各 `Workspace` が個別の `initial_cwd` を持つ設計のため、  
   タブをまたいだ攻撃ベクタは存在しない。ただし、新規タブ作成時の cwd は  
   `std::env::current_dir()` に依存するため、起動後の環境変数操作の影響を受ける可能性がある。

---

## 自己評価

- **修正の堅牢性(自己採点)**: 8 / 10
- **採点理由**:
  - `canonicalize()` + `starts_with()` の組み合わせは、symlink・相対参照・`../` を使った  
    迂回を全て排除できる（symlink は resolve 済みのパスで比較されるため）。
  - 変更量が最小限（+18行）で、既存テストを全て通過し、副作用が明確に把握できている。
  - 残点 2 点は「意図的な別プロジェクトへの cd」が機能しなくなる UX コスト分。  
    将来の確認ダイアログ実装で 9-10 点に改善可能。
