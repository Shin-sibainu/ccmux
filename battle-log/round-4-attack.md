# Round 4 - Attack Report

## 脆弱性
- **種別**: Unvalidated External Input Displayed in UI (UI Spoofing / Social Engineering via npm Registry)
- **OWASP分類**: A08:2021 - Software and Data Integrity Failures
- **深刻度**: Medium
- **CVSS推定**: 5.3 (AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:L/A:L)

## 該当箇所
- ファイル: `src/version_check.rs`
- 行番号: L48-L58
- 該当コード(抜粋):

```rust
fn fetch_latest() -> Result<String, Box<dyn std::error::Error>> {
    let response = ureq::get("https://registry.npmjs.org/ccmux-cli/latest")
        .timeout(Duration::from_secs(5))
        .call()?;
    let json: serde_json::Value = response.into_json()?;
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("no version field")?
        .to_string();
    Ok(version)
}
```

- ファイル: `src/version_check.rs`
- 行番号: L20-L28
- 該当コード(抜粋):

```rust
pub fn update_available(&self) -> Option<String> {
    let latest = self.inner.lock().ok()?.clone()?;
    if is_newer(&latest, CURRENT_VERSION) {
        Some(latest)
    } else {
        None
    }
}
```

- ファイル: `src/ui.rs`
- 行番号: L948-L956
- 該当コード(抜粋):

```rust
if let Some(new_version) = app.version_info.update_available() {
    right_spans.push(Span::styled(
        format!(" \u{2191} v{} ", new_version),
        Style::default()
            .fg(ACCENT_CLAUDE)
            .add_modifier(Modifier::BOLD),
    ));
}
```

## 攻撃シナリオ(概念説明のみ)

ccmux は起動時にバックグラウンドスレッドで npm レジストリ (`https://registry.npmjs.org/ccmux-cli/latest`) にアクセスし、最新バージョン情報を取得する。取得した JSON レスポンスの `version` フィールドは文字列としてそのまま保存され、以下の2点について検証が行われていない。

### 問題1: version 文字列の内容・長さが未検証

`fetch_latest()` は `version` フィールドの値を文字列として取得するだけで、セマンティックバージョニング形式（`X.Y.Z`）であることの検証、長さの制限、許可文字のチェックのいずれも行っていない。

`is_newer()` 関数はバージョン文字列を `.` で分割し `parse::<u32>()` で数値変換を試みるが、変換に失敗した部分は `filter_map` で**黙って無視**される。このため、`"99.0.0 CRITICAL: run npm install ccmux-backdoor"` のような文字列は `[99, 0]` としてパースされ、現行バージョン `[0, 6, 1]` より新しいと判定される。結果として、この任意テキストを含む文字列全体が `update_available()` から返却され、ステータスバーに表示される。

npm パッケージの `version` フィールドは npm registry の publish 時に設定される。npm アカウントが侵害された場合（2FA未設定、トークン漏洩、dependency confusion 等 -- ua-parser-js, event-stream, colors.js の事例参照）、攻撃者はこのフィールドに任意の文字列を設定できる。

ステータスバーの表示は `ACCENT_CLAUDE` 色（オレンジ）+ **太字**でレンダリングされ、ccmux の公式通知と視覚的に区別がつかない。ユーザーは更新通知に見せかけたメッセージ（「v2.0.0 セキュリティ修正: npm i -g ccmux-cli@latest を実行してください」等）を信頼し、悪意あるパッケージをインストールする可能性がある。

### 問題2: HTTP レスポンスボディのサイズ制限がない

`response.into_json()` はレスポンスボディ全体をメモリに読み込んで serde_json でパースする。レスポンスサイズの上限チェックがないため、5秒のタイムアウト内に受信可能なデータ量（高速回線で数十MB以上）がそのままメモリに展開される。これはバックグラウンドスレッドで実行されるため、失敗しても ccmux 本体はクラッシュしないが、一時的なメモリ圧迫を引き起こす可能性がある。

## 想定される影響

- **なりすまし/ソーシャルエンジニアリング**: ステータスバーに表示される偽の更新通知により、ユーザーが悪意あるコマンドを実行するよう誘導される。ccmux のすべてのアクティブインスタンスに同時に表示されるため、影響範囲が広い。
- **一時的なメモリ消費**: 異常に大きなレスポンスにより、バックグラウンドスレッドが数十MBのメモリを消費する可能性がある。
- **u16 オーバーフロー**: ui.rs L959 で `total_width` を `u16` として計算しているため、極端に長い version 文字列でオーバーフローし、レイアウト計算が不正になる可能性がある。

## Defender への要求

### 修正方針

1. **version 文字列のバリデーション**: `fetch_latest()` で取得した version 文字列に対し、セマンティックバージョニング形式の簡易チェックを行う。具体的には:
   - 文字列長の上限（例: 20文字）
   - 許可文字の制限（数字、`.`、`-`、英字のみ）
   - 正規表現またはパターンマッチで `X.Y.Z` 形式を検証

2. **レスポンスサイズの制限**: `into_json()` の前に `response` のサイズを確認するか、`ureq` の設定でレスポンスサイズ上限を設ける（例: 1MB）。

3. **表示時のトランケーション**: `ui.rs` での表示時に version 文字列を一定長（例: 20文字）で切り詰め、UI 上でのスプーフィング可能範囲を限定する。

### 修正の妥当性を確認するための観点

- 正常なセマンティックバージョニング文字列（`0.7.0`, `1.0.0-beta.1`）が正しく表示されること。
- 不正な文字列（任意テキスト、極端に長い文字列、空文字列）がフィルタリングまたは切り詰められること。
- version チェックが失敗した場合に、ステータスバーに何も表示されないこと（既存動作の維持）。
- `is_newer()` がバリデーション済みの文字列のみを受け取ること。

## 過去ラウンドとの差分
- **直前ラウンドの修正内容**: Round 3 では `image::Limits` API を使い、画像プレビュー時のデコード制限（64MB alloc, 8192px max width/height）を追加し、Decompression Bomb を防止した。
- **今回の指摘がそれとどう異なるか**: 新規。Round 1-3 はすべて PTY 出力やローカルファイルシステム経由の攻撃だったが、今回は**外部ネットワークからのレスポンス**を攻撃ベクタとする初めてのカテゴリ。version_check.rs は起動時に npm レジストリにアクセスし、レスポンスを検証なしに UI 表示する。npm サプライチェーン攻撃は実例が豊富であり、信頼できない外部入力の検証不足として独立した脆弱性である。
