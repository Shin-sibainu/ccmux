# Round 6 - Defense Report

## 対応した脆弱性
Window Title Spoofing (OSC 0/2 によるペーン ID 偽装)
- OWASP分類: A07:2021 - Identification and Authentication Failures

## 修正方針
`is_claude_running()` の判定条件を `contains("claude")` から `starts_with("claude code")` に変更した。

Claude Code は OSC 0/2 でターミナルタイトルを以下のパターンで設定する:
- `"Claude Code"`
- `"Claude Code - /path/to/project"`

いずれも `"Claude Code"` で始まるため、`starts_with("claude code")`（既に `to_lowercase()` 適用済み）で正確にマッチできる。

### 検討した他の選択肢

1. **`contains("claude code")` にする案**: 部分一致のままなので、タイトルの途中に `"Claude Code"` を埋め込む攻撃が通る。`starts_with` のほうが安全。却下。

2. **ClaudeMonitor の JSONL セッション検出と組み合わせる二重チェック案**: `is_claude_running()` の結果を JSONL ファイルの存在で裏付ける。最も堅牢だが、コンポーネント間の結合度が上がり変更量が大きい。今回の指摘に対しては過剰修正。却下。

3. **`starts_with("claude code")` にする案（採用）**: 1行の変更で攻撃の大半を防げる。Claude Code の正規のタイトル設定パターンに合致し、誤検出リスクが低い。

## 変更内容
- ファイル: `src/pane.rs`
- 変更行数: +7 / -1
- コミットハッシュ: 2e476684b29500bc09054b9f5e40a1c57b4ddd4b

```diff
-            lower.contains("claude")
+            lower.starts_with("claude code")
```

## 副作用確認
- 既存テスト: 全34件通過
- ビルド確認: `cargo build` 成功
- 動作確認:
  - タイトルが `"Claude Code"` の場合: `starts_with("claude code")` -> true (正常検出)
  - タイトルが `"Claude Code - /home/user/project"` の場合: true (正常検出)
  - タイトルが `"claude"` の場合: `starts_with("claude code")` -> false (攻撃ブロック)
  - タイトルが `"my-claude-tool"` の場合: false (攻撃ブロック)
  - タイトルが空の場合: false (正常)
  - タイトルが `"vim"` 等の一般的 TUI の場合: false (正常)

## 残留リスク(あれば)

1. **攻撃者が `printf '\033]0;Claude Code\007'` と正確なプレフィックスを送る場合**: `starts_with("claude code")` をパスする。タイトルベースの判定である限り、正確なタイトルを知っている攻撃者には対抗できない。完全な防御には JSONL セッション検出との二重チェックが必要。

2. **`extract_osc_title` 自体は依然として任意のタイトル文字列を受け入れる**: タイトルの保存自体にはサニタイズが無い。今回の修正は判定側（消費側）のみの対策であり、格納側は未対策。ただし現状タイトル文字列はUI表示には使われず `is_claude_running()` の判定にのみ使われるため、実害は限定的。

3. **Claude Code のタイトル形式が将来変更された場合**: `starts_with("claude code")` が機能しなくなる可能性がある。その場合はパターンの更新が必要。

## 自己評価
- 修正の堅牢性(自己採点): 6/10
- 採点理由: 1行の変更で「claude」だけの単純な偽装を防止できるが、攻撃者が正確なプレフィックス `"Claude Code"` を知っていれば依然として突破可能。タイトルベースの判定という本質的な限界は残る。ただし、攻撃のハードルを上げることには成功しており、攻撃レポートで示された `printf '\033]0;claude\007'` の攻撃シナリオは確実にブロックされる。
