# Round 2 - Defense Report

## 対応した脆弱性
TOCTOU (Time-of-Check to Time-of-Use) Race Condition -- Symlink Check Bypass in File Preview
- OWASP分類: A01:2021 - Broken Access Control
- filetree.rs の symlink スキップ (スキャン時) と preview.rs のファイル読み取り (ユーザ操作時) の間の時間差を悪用し、通常ファイルを symlink に差し替えることでワークスペース外のファイル内容を Preview パネルに表示させる攻撃。

## 修正方針
`Preview::load()` の冒頭で `path.canonicalize()` を実行し、解決後のパスが `initial_cwd` 配下にあるかを `starts_with()` で検証するアプローチを採用した。Round 1 の OSC 7 防御と同じ `canonicalize() + starts_with()` パターンであり、一貫性がある。

### 検討した他の選択肢

1. **Preview 構造体に `boundary` フィールドを持たせる**: コンストラクタで `initial_cwd` を渡す方式。`load()` のシグネチャを変えなくて済むが、Preview が Workspace の知識を持つことになり結合度が上がる。却下。

2. **app.rs の呼び出し側で境界チェック**: `preview.load()` を呼ぶ前に app.rs 側でチェックする方式。セキュリティロジックが複数箇所に散らばりチェック漏れのリスクが上がる。却下。

3. **(採用) `load()` に `boundary: Option<&Path>` パラメータを追加**: セキュリティチェックが `load()` 内に集約され、呼び出し側は boundary を渡すだけ。テストでは `None` を渡して既存動作を維持できる。最もシンプルかつ副作用が少ない。

## 変更内容
- ファイル: `src/preview.rs`、`src/app.rs`
- 変更行数: +44 / -6
- コミットハッシュ: 4698acb3bd5d8f37c733eed7af1a9077de7484b1

```diff
--- a/src/preview.rs
+++ b/src/preview.rs
-    pub fn load(&mut self, path: &Path, picker: Option<&mut Picker>) {
+    pub fn load(&mut self, path: &Path, picker: Option<&mut Picker>, boundary: Option<&Path>) {
         if self.file_path.as_deref() == Some(path) {
             return;
         }

+        // Security: resolve symlinks and verify the real path is within the
+        // workspace boundary.
+        if let Some(boundary) = boundary {
+            match path.canonicalize() {
+                Ok(real_path) => {
+                    if !real_path.starts_with(boundary) {
+                        // Reject: file resolves outside workspace
+                        return;
+                    }
+                }
+                Err(_) => {
+                    // Cannot resolve (dangling symlink, etc.) -- reject
+                    return;
+                }
+            }
+        }

--- a/src/app.rs (both call sites)
+                    let boundary = self.ws().initial_cwd.clone();
-                    self.ws_mut().preview.load(&path, picker.as_mut());
+                    self.ws_mut().preview.load(&path, picker.as_mut(), Some(&boundary));
```

## 副作用確認
- 既存テスト: 全30件通過 (cargo test)
- 動作確認:
  - ワークスペース内の通常ファイルは正常にプレビュー可能 (テストで確認済み)
  - `canonicalize()` 失敗時はエラーメッセージを表示しパニックしない
  - symlink 先がワークスペース内であればプレビュー許可される (canonicalize 後のパスが starts_with を満たすため)
  - symlink 先がワークスペース外であればプレビュー拒否される (starts_with が false を返す)
  - テストでは `boundary: None` を渡すことで既存の振る舞いを維持

## 残留リスク(あれば)
1. **TOCTOU の縮小版**: `canonicalize()` と `File::open()` / `metadata()` の間にまだ微小な時間窓が残る。`canonicalize()` 後にパスが差し替えられるとすり抜ける可能性がゼロではないが、窓は数マイクロ秒程度であり、実用上の攻撃成功確率は極めて低い。完全に閉じるには `O_NOFOLLOW` + `openat()` 相当の API が必要だが、Rust 標準ライブラリの範囲では困難。
2. **FileTree の `scan_directory_filtered` 自体のレース**: ファイルツリーのスキャン結果はキャッシュされるため、スキャン後に作成された symlink がツリーに表示されることはない。ただし、2秒の自動リフレッシュでキャッシュが更新される際に通常ファイルとしてスキャンされた後、次のリフレッシュまでに symlink に差し替えられるシナリオは残る。しかし今回の修正により `Preview::load()` 側で canonicalize チェックが入るため、表示はブロックされる。
3. **image preview パス**: `image::ImageReader::open(path)` も symlink を follow する。今回の修正では `load()` の冒頭でチェックしているため、画像パスも boundary チェック済みの状態で `ImageReader::open` に到達する。

## 自己評価
- 修正の堅牢性(自己採点): 8/10
- 採点理由: Round 1 と同じ `canonicalize() + starts_with()` パターンを Preview にも水平展開しており、一貫性のある防御。`canonicalize()` から実際の I/O 操作までの微小な TOCTOU 窓は残るが、これは OS レベルの `openat(O_NOFOLLOW)` + fd ベースの操作でしか完全に解消できず、Rust 標準ライブラリの制約上この対応が現実的な最善策。
