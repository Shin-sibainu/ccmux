# Round 2 - Attack Report

## 脆弱性
- **種別**: TOCTOU (Time-of-Check to Time-of-Use) Race Condition — Symlink Check Bypass in File Preview
- **OWASP分類**: A01:2021 - Broken Access Control
- **深刻度**: Medium
- **CVSS推定**: 5.5 (AV:L/AC:L/PR:N/UI:R/S:U/C:H/I:N/A:N — ローカル攻撃、ユーザ操作要、機密性への高影響)

## 該当箇所

### チェック箇所: `src/filetree.rs` L82-L87
```rust
// Skip symlinks to prevent traversal outside the project
if let Ok(meta) = entry_path.symlink_metadata() {
    if meta.is_symlink() {
        continue;
    }
}
```

### 使用箇所: `src/preview.rs` L62-L147 (`Preview::load`)
```rust
pub fn load(&mut self, path: &Path, picker: Option<&mut Picker>) {
    // ... path に対する境界チェックなし ...
    let metadata = match std::fs::metadata(path) {  // symlink を follow する
        Ok(m) => m,
        Err(_) => { /* ... */ return; }
    };
    // ...
    match File::open(path) {  // symlink を follow する
        Ok(file) => {
            let reader = BufReader::new(file);
            self.lines = reader
                .lines()
                .take(MAX_PREVIEW_LINES)  // 500行まで読む
                .filter_map(|l| l.ok())
                .collect();
        }
        // ...
    }
}
```

### 呼び出しチェーン: `src/app.rs` L791-L797, L1252-L1257
```rust
// FileTree でファイルを選択 → Preview に直接渡す
let path = self.ws_mut().file_tree.toggle_or_select();
if let Some(path) = path {
    self.clear_selection_if_preview();
    let mut picker = self.image_picker.take();
    self.ws_mut().preview.load(&path, picker.as_mut());  // パス検証なし
    self.image_picker = picker;
}
```

## 攻撃シナリオ(概念説明のみ)

この脆弱性は、`filetree.rs` の symlink チェック（スキャン時）と `preview.rs` のファイル読み取り（ユーザ操作時）の間に時間差が存在することを悪用する TOCTOU レースコンディションである。

1. PTY ペイン内で実行中の悪意あるプログラム（npm postinstall スクリプト、cargo build スクリプト等）が、ワークスペース内に通常ファイル（例: `src/config.txt`）を作成する。
2. FileTree の自動リフレッシュ（2秒間隔）でこのファイルがツリーに表示される。`scan_directory_filtered` の `symlink_metadata()` チェックは通常ファイルなので通過する。
3. ファイルがツリーにキャッシュされた直後、攻撃プログラムはこのファイルを削除し、同名の symlink（`~/.ssh/id_rsa`、`~/.aws/credentials`、`~/.claude/settings.json` 等を指す）に置き換える。
4. FileTree のキャッシュには「通常ファイル」として残っている。次の自動リフレッシュまで最大2秒の猶予がある。
5. ユーザがこのファイルをクリック（または Enter で選択）すると、`Preview::load()` が呼ばれる。
6. `Preview::load()` は `std::fs::metadata()` と `File::open()` を使用する。これらは symlink を follow するため、symlink 先の実ファイルの内容（最大500行）がプレビューパネルに表示される。

**補足**: 自動リフレッシュが symlink を検出して削除しても、攻撃プログラムは再び通常ファイルに戻し、次のサイクルで再度 symlink に置き換えることを繰り返せる。また、`Preview::load()` には `initial_cwd` との境界チェックが一切ないため、symlink 先がワークスペース外であっても読み取りが成功する。

## 想定される影響

- **機密ファイル内容の開示**: SSH 秘密鍵、AWS 認証情報、`.env` ファイル、`.claude/settings.json` 等の内容が最大500行まで Preview パネルに表示される。
- **Round 1 修正の迂回**: Round 1 で `initial_cwd` 境界チェックが OSC 7 パスに追加されたが、`Preview::load()` にはこのチェックが適用されていない。FileTree のルート自体は変更できなくても、個別ファイルの読み取りパスには防御がない。

## Defender への要求

### 修正方針

1. **`Preview::load()` にパス境界チェックを追加する**
   - `Preview::load()` の冒頭で、渡されたパスを `canonicalize()` し、`initial_cwd` の配下であることを確認する。
   - `canonicalize()` は symlink を解決した後の実パスを返すため、symlink 経由のパストラバーサルを防止できる。
   - `initial_cwd` は `Workspace` から渡す必要があるため、`Preview::load()` のインターフェースに `boundary: &Path` 引数を追加するか、`Preview` 構造体に境界パスを保持する。

2. **ファイルオープン直前に `symlink_metadata()` で再チェックする**（防御の多層化）
   - `Preview::load()` 内で `File::open()` の前に `path.symlink_metadata()` を確認し、symlink であれば読み取りを拒否する。
   - これは TOCTOU の窓を完全には閉じないが、攻撃の難易度を上げる。

推奨は方針1（canonicalize + starts_with）。Round 1 で OSC 7 パスに適用したのと同じパターンを Preview にも適用する形になる。

### 修正の妥当性を確認するための観点

- 修正後、ワークスペース内の通常ファイルが正常にプレビューできること。
- ワークスペース内に symlink がある場合、その symlink 先がワークスペース外であればプレビューが拒否されること。
- ワークスペース内に symlink があり、symlink 先もワークスペース内であればプレビューが許可されること（正当なユースケース）。
- `canonicalize()` が失敗するケース（ファイルが存在しない、権限がない等）でパニックしないこと。

## 過去ラウンドとの差分
- **直前ラウンドの修正内容**: Round 1 では `AppEvent::CwdChanged` ハンドラに `initial_cwd` 境界チェックを追加し、OSC 7 経由の FileTree ルート変更を `initial_cwd` 配下に制限した。
- **今回の指摘がそれとどう異なるか**: Round 1 の修正は FileTree の「ルートディレクトリ」の変更を防いだが、FileTree 内の「個別ファイル」の読み取り（Preview）にはパス検証が適用されていない。攻撃ベクタが異なる（OSC 7 → symlink TOCTOU）が、防御パターンは共通（canonicalize + starts_with）である。Round 1 の防御を Preview レイヤーにも水平展開する必要がある。
