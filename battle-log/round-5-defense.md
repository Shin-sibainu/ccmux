# Round 5 - Defense Report

## 対応した脆弱性
Bracketed Paste Escape Injection (A03:2021 - Injection)
ペースト内容にブラケットペースト終了シーケンス `\x1b[201~` を注入することで、ペースト境界を脱出し、任意のコマンドを通常入力として実行させる攻撃。

## 修正方針
ペースト内容をブラケットペーストシーケンスで囲む直前に、内容からブラケットペースト関連シーケンス (`\x1b[200~` と `\x1b[201~`) を除去するサニタイズ関数を適用する。

これは xterm, iTerm2, WezTerm 等の主要ターミナルエミュレータが採用している標準的な防御手法であり、最もシンプルかつ実績のあるアプローチ。

検討した他の選択肢:
1. **ペースト内容全体のエスケープシーケンスを除去する** -- 過剰防御。正当な ESC シーケンスを含むテキスト（ANSIカラーコード等）のペーストが壊れる。却下。
2. **ブラケットペースト終了シーケンスをエスケープ/エンコードして保持する** -- 受信側アプリケーションとの互換性の問題が生じる。主要ターミナルは「除去」方式を採用しており、それに倣うのが最も安全。却下。
3. **ブラケットペースト開始/終了シーケンスのみを除去する（採用）** -- 最小限の変更で、主要ターミナルと同じ防御を実現。正当なペースト内容には影響なし。

## 変更内容
- ファイル: `src/app.rs`, `src/main.rs`
- 変更行数: +68 / -4
- コミットハッシュ: f12dfc2d10fc6f8c403651e6f3334ab1927fc9cb

```diff
diff --git a/src/app.rs b/src/app.rs
--- a/src/app.rs
+++ b/src/app.rs
+/// Strip bracketed paste escape sequences from paste content.
+pub fn strip_bracket_paste_sequences(input: &[u8]) -> Vec<u8> {
+    const START: &[u8] = b"\x1b[200~";
+    const END: &[u8] = b"\x1b[201~";
+    let mut out = Vec::with_capacity(input.len());
+    let mut i = 0;
+    while i < input.len() {
+        if input[i] == 0x1b
+            && (input[i..].starts_with(START) || input[i..].starts_with(END))
+        {
+            i += 6;
+        } else {
+            out.push(input[i]);
+            i += 1;
+        }
+    }
+    out
+}

 // forward_paste_to_pty: sanitize before wrapping
-                data.extend_from_slice(text.as_bytes());
+                let sanitized = strip_bracket_paste_sequences(text.as_bytes());
+                data.extend_from_slice(&sanitized);

diff --git a/src/main.rs b/src/main.rs
 // flush_paste_buffer: sanitize before wrapping
-                data.extend_from_slice(buffer);
+                let sanitized = app::strip_bracket_paste_sequences(buffer);
+                data.extend_from_slice(&sanitized);
```

## 副作用確認
- 既存テスト: 全34件通過（新規追加1件含む）
- 動作確認:
  - 通常テキスト（エスケープシーケンスなし）のペーストに影響がないことをテストで確認
  - ブラケットペースト無効時のパスはサニタイズを適用しない（不要なため）ことを確認
  - 部分的なエスケープシーケンス（`\x1b[20` 等）が誤って除去されないことをテストで確認
  - 複数回の出現、連続した出現が正しく除去されることをテストで確認
  - 攻撃ペイロード（`safe\x1b[201~malicious\n\x1b[200~rest`）が正しくサニタイズされることをテストで確認

## 残留リスク(あれば)
- **他のエスケープシーケンス注入**: 今回の修正はブラケットペーストシーケンスのみを対象としている。ペースト内容に含まれる他のエスケープシーケンス（OSC, DCS 等）はサニタイズしていない。ただし、これらは PTY アプリケーション側（readline, vim 等）が処理する責任範囲であり、ターミナルマルチプレクサとしては標準的な振る舞い。
- **非ブラケットペーストモード時のペースト**: ブラケットペーストが無効な場合、ペースト内容はそのまま PTY に送信される。これは仕様通りだが、シェルがブラケットペーストを有効にしていない環境ではペーストジャッキング対策が働かない（ブラケットペースト自体がセキュリティ機構のため、無効時は元々保護対象外）。

## 自己評価
- 修正の堅牢性(自己採点): 9
- 採点理由: 主要ターミナルエミュレータ（xterm, iTerm2, WezTerm）と同一のアプローチを採用しており、実績のある防御手法。バイト単位のスキャンで `\x1b[200~` / `\x1b[201~` の完全一致のみを除去するため、偽陽性（正当なテキストの破壊）のリスクが極めて低い。両方のペースト経路を漏れなくカバーし、ユニットテストで攻撃パターンを含む複数ケースを検証済み。10にしなかった理由は、他のエスケープシーケンス注入の残留リスクがあるため。
