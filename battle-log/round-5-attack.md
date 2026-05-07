# Round 5 - Attack Report

## 脆弱性
- **種別**: Bracketed Paste Escape Injection (ペースト内容にブラケットペースト終了シーケンスを注入)
- **OWASP分類**: A03:2021 - Injection
- **深刻度**: High
- **CVSS推定**: 7.4 (AV:N/AC:L/PR:N/UI:R/S:C/C:N/I:H/A:N)

## 該当箇所

### 箇所1: `Event::Paste` ハンドラ経由のペースト
- ファイル: `src/app.rs`
- 行番号: L1600-L1615
- 該当コード(抜粋):

```rust
pub fn forward_paste_to_pty(&mut self, text: &str) -> Result<()> {
    let focused_id = self.ws().focused_pane_id;
    if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
        pane.scroll_reset();
        if pane.is_bracketed_paste_enabled() {
            let mut data = Vec::with_capacity(text.len() + 12);
            data.extend_from_slice(b"\x1b[200~");
            data.extend_from_slice(text.as_bytes()); // 未サニタイズ
            data.extend_from_slice(b"\x1b[201~");
            pane.write_input(&data)?;
        } else {
            pane.write_input(text.as_bytes())?;
        }
    }
    Ok(())
}
```

### 箇所2: キーイベント蓄積によるペースト検出経由
- ファイル: `src/main.rs`
- 行番号: L204-L230
- 該当コード(抜粋):

```rust
fn flush_paste_buffer(app: &mut app::App, buffer: &mut Vec<u8>) -> Result<()> {
    // ...
    if buffer.len() > 6 {
        if pane.is_bracketed_paste_enabled() {
            let mut data = Vec::with_capacity(buffer.len() + 12);
            data.extend_from_slice(b"\x1b[200~");
            data.extend_from_slice(buffer); // 未サニタイズ
            data.extend_from_slice(b"\x1b[201~");
            pane.write_input(&data)?;
        } else {
            pane.write_input(buffer)?;
        }
        // ...
    }
    // ...
}
```

## 攻撃シナリオ(概念説明のみ)

ブラケットペーストモード（Bracketed Paste Mode）は、ターミナルアプリケーション（readline, vim, Claude Code 等）がペースト入力と通常のキーボード入力を区別するための仕組みである。ペースト内容は `\x1b[200~`（開始）と `\x1b[201~`（終了）のエスケープシーケンスで囲まれ、受信側アプリケーションはこの範囲内のテキストを「ペーストされたもの」として一括処理する。

ccmux は両方のペースト経路（`forward_paste_to_pty` と `flush_paste_buffer`）で、ペースト内容をそのままブラケットペーストシーケンスで囲んで PTY に送信する。しかし、**ペースト内容自体にブラケットペースト終了シーケンス `\x1b[201~` が含まれている場合の処理が行われていない**。

攻撃の流れ:

1. 攻撃者は、ユーザーがコピーする可能性のあるテキスト（Webページ、ドキュメント、チャットメッセージ等）に、不可視のエスケープシーケンスを埋め込む。具体的には、表示上は無害なテキストの中に `\x1b[201~` を含め、その後に任意のコマンドシーケンスを配置する。

2. ユーザーがこのテキストをクリップボードにコピーし、ccmux 内のブラケットペースト対応アプリケーション（Claude Code, bash readline 等）にペーストする。

3. ccmux は内容を `\x1b[200~...内容...\x1b[201~` で囲んで PTY に送信するが、内容中の `\x1b[201~` により、受信側アプリケーションはその時点でペーストが終了したと解釈する。

4. `\x1b[201~` 以降のバイト列は、ペースト外の通常入力として処理される。readline であればコマンドとして実行され、vim であればノーマルモードのコマンドとして解釈される。

この攻撃は「ペーストジャッキング」の一種であり、xterm, iTerm2, WezTerm 等の主要ターミナルエミュレータではペースト内容からブラケットペーストシーケンスを除去するサニタイズ処理が実装されている。ccmux にはこの防御が欠如している。

## 想定される影響

- **任意コマンド実行**: ペースト先が bash/zsh のプロンプトである場合、注入された内容がシェルコマンドとして実行される。ユーザーの権限で任意のコマンドが実行可能。
- **Claude Code への不正プロンプト注入**: ペースト先が Claude Code である場合、注入された内容がユーザー入力として処理され、意図しない操作（ファイル変更、コマンド実行等）を Claude に指示できる。
- **なりすまし**: ユーザーは自分がペーストした内容のみが入力されたと認識するが、実際には追加のコマンドが実行されている。

## Defender への要求

### 修正方針

両方のペースト経路（`forward_paste_to_pty` と `flush_paste_buffer`）で、ブラケットペーストシーケンスで囲む前に、ペースト内容からブラケットペースト関連のエスケープシーケンス（`\x1b[200~` と `\x1b[201~`）を除去する。

主要ターミナルエミュレータの実装例:
- xterm: ペースト内容から `\x1b[200~` と `\x1b[201~` をストリップ
- WezTerm: 同様のストリップ処理
- iTerm2: 同様のストリップ処理

実装方針としては:
1. `text.as_bytes()` または `buffer` を PTY に書き込む前に、`\x1b[200~` および `\x1b[201~` のバイト列を検索・除去するユーティリティ関数を用意する
2. `forward_paste_to_pty` と `flush_paste_buffer` の両方でこの関数を適用する

### 修正の妥当性を確認するための観点

- ペースト内容にブラケットペースト終了シーケンスを含むテキストをペーストした場合、終了シーケンスが除去され、残りの内容が正常にペーストされること
- ブラケットペースト開始シーケンスもペースト内容から除去されること（入れ子防止）
- 正常なペースト（エスケープシーケンスを含まない通常テキスト）の動作に影響がないこと
- ブラケットペーストが無効なシェルへのペースト動作に影響がないこと
- サニタイズは PTY への書き込み直前に行い、クリップボードの内容自体は変更しないこと

## 過去ラウンドとの差分
- **直前ラウンドの修正内容**: Round 4 では `fetch_latest()` に `is_valid_semver()` を追加し、npm レジストリから取得したバージョン文字列のバリデーションを実装した。
- **今回の指摘がそれとどう異なるか**: 新規。Round 1-4 はそれぞれ OSC 7 パス、symlink TOCTOU、画像 decompression bomb、npm バージョン文字列というローカルファイルシステムまたは外部 HTTP 入力の脆弱性だった。今回は**クリップボード経由のエスケープシーケンス注入**という新しい攻撃ベクタであり、PTY への書き込みパスにおけるサニタイズ不足を指摘している。ブラケットペーストモードは端末アプリケーションのセキュリティ境界として機能するため、この境界を破る攻撃はインジェクションに分類される。
