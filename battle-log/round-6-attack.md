# Round 6 - Attack Report

## 結論: 脆弱性なし

High 以上の深刻度を持つ新規脆弱性は発見されなかった。

## チェックした観点と確認結果

### 1. エスケープシーケンス注入 (PTY 入力パス)
- **確認箇所**: `forward_paste_to_pty` (app.rs L1622-1638)、`flush_paste_buffer` (main.rs L204-231)
- **結果**: Round 5 の修正により、ブラケットペーストシーケンス (`\x1b[200~` / `\x1b[201~`) が両経路でサニタイズされている。非ブラケットペーストモード時の生テキスト送信は端末の標準仕様通りであり、ccmux 固有の脆弱性ではない。

### 2. エスケープシーケンス注入 (PTY 出力パス)
- **確認箇所**: `pty_reader_thread` (pane.rs L252-297)、`extract_osc7`、`extract_osc_title`
- **結果**: PTY 出力は vt100 パーサに入力され、レンダリングは ratatui 経由で行われる。生のエスケープシーケンスがホスト端末に転送されることはない。OSC 7 パスは canonicalize + initial_cwd 境界チェックで保護済み（Round 1）。OSC 0/2 タイトルは `starts_with("claude code")` で判定強化済み（Round 6 旧）。

### 3. ファイルプレビュー境界チェック
- **確認箇所**: `Preview::load` (preview.rs L67-101)、`handle_file_tree_key` (app.rs L812-818)、マウスクリック経由 (app.rs L1275-1281)
- **結果**: 全てのプレビュー呼び出しパスで `boundary: Some(&initial_cwd)` が渡されている。`canonicalize()` によるシンボリックリンク解決と `starts_with()` 境界チェックが実装済み。canonicalize から File::open までの微小 TOCTOU 窓は Round 2 で報告・確認済みであり、OS レベル (`openat(O_NOFOLLOW)`) でしか完全解消できない性質のもの。

### 4. 画像デコード
- **確認箇所**: preview.rs L135-156
- **結果**: Round 3 の修正により `image::Limits` が設定されている (max_alloc: 64MB、max_width/height: 8192px)。ファイルサイズ上限 (20MB) も事前チェック済み。

### 5. 外部 HTTP 入力 (npm バージョンチェック)
- **確認箇所**: version_check.rs L84-98
- **結果**: Round 4 の修正により `is_valid_semver()` バリデーションが追加されている。semver 形式以外の文字列（スペース、大文字、過度に長い文字列等）は拒否される。

### 6. シェル起動・コマンド注入
- **確認箇所**: `Pane::new_with_cwd` (pane.rs L34-128)、`detect_shell` (pane.rs L370-425)
- **結果**: シェル初期化コマンド (OSC 7 フック) は `concat!` によるコンパイル時定数であり、ユーザー入力の動的補間は行われていない。`$HOSTNAME` / `$PWD` はシェル変数としてシェル内で展開されるため、Rust 側での注入リスクはない。`cmd.cwd()` は OS レベルの chdir 相当であり、シェル経由ではない。

### 7. ファイルツリー走査
- **確認箇所**: `scan_directory_filtered` (filetree.rs L51-107)
- **結果**: シンボリックリンクは `symlink_metadata()` で検出・スキップされている。ディレクトリエントリ数上限 (500件) によるリソース制限も実装済み。

### 8. スレッド安全性
- **確認箇所**: parser / title の `Arc<Mutex<T>>` パターン（pane.rs 全体、ui.rs）
- **結果**: ロック取得は `unwrap_or_else(|e| e.into_inner())` で毒化ミューテックスに対応。ロック順序の逆転によるデッドロックリスクは確認されなかった（reader スレッドは parser ロックのみ、メインスレッドはレンダリング中に parser ロックのみ）。

### 9. クリップボード操作
- **確認箇所**: `copy_to_clipboard` (app.rs L471-478)、`extract_selected_text` (app.rs L1727-1756)
- **結果**: テキスト選択はvt100スクリーンバッファのセル内容を読み取る。エスケープシーケンスはパーサ段階で処理済みであり、クリップボードにはレンダリング後の可視テキストのみが入る。

### 10. JSONL モニタリング
- **確認箇所**: `claude_monitor.rs` 全体
- **結果**: JSONL は `~/.claude/projects/` 下のユーザー所有ディレクトリから読み取られる。パース済み文字列は ratatui のウィジェットとして表示されるため、端末エスケープシーケンスとして解釈されることはない。文字列の長さ制限は明示的には実装されていないが、UI 表示領域がクランプされるため実質的な影響はない。

## 過去ラウンドとの差分
- 直前ラウンドの修正内容: Round 6（旧）で `is_claude_running()` の判定を `contains("claude")` から `starts_with("claude code")` に変更した。
- 今回の結果: 6 ラウンドの修正を経て、主要な攻撃面（PTY 入出力のエスケープシーケンス、ファイルシステムアクセス、外部入力バリデーション）が一貫して防御されている。残存するリスクは Medium 以下の項目のみであり、High 以上の新規脆弱性は発見されなかった。
