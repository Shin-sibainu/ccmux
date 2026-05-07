# Round 3 - Attack Report

## 脆弱性
- **種別**: Decompression Bomb (Image Pixel Flood) in File Preview
- **OWASP分類**: A05:2021 - Security Misconfiguration (リソース制限の欠如)
- **深刻度**: Medium
- **CVSS推定**: 5.5 (AV:L/AC:L/PR:N/UI:R/S:U/C:N/I:N/A:H)

## 該当箇所
- ファイル: `src/preview.rs`
- 行番号: L124-L149
- 該当コード(抜粋):

```rust
// Try loading as image first (by extension)
if is_image_extension(path) {
    if metadata.len() > MAX_IMAGE_SIZE {
        // ... reject files > 20MB on disk ...
        return;
    }
    if let Some(picker) = picker {
        match image::ImageReader::open(path)
            .and_then(|r| r.with_guessed_format())
            .map_err(|e| e.to_string())
            .and_then(|r| r.decode().map_err(|e| e.to_string()))
        {
            Ok(dyn_img) => {
                self.image_protocol = Some(picker.new_resize_protocol(dyn_img));
                return;
            }
            Err(_) => {
                // Fall through to text/binary preview
            }
        }
    }
}
```

## 攻撃シナリオ(概念説明のみ)

この脆弱性は、画像ファイルの**圧縮後サイズ**（ディスク上のバイト数）と**展開後サイズ**（ピクセルデータのメモリ上のバイト数）の乖離を悪用する。いわゆる「デコンプレッション・ボム」（Pixel Flood）攻撃である。

1. 攻撃者は PTY ペイン内で実行中のプログラム（npm postinstall スクリプト、cargo build スクリプト等）を通じて、ワークスペース内に細工された PNG ファイルを配置する。このファイルはディスク上では数 KB～数 MB だが、画像ヘッダには極端に大きなピクセル寸法（例: 65535x65535 ピクセル = 約 43 億ピクセル）が記録されている。均一な色のピクセルデータは PNG の deflate 圧縮で極めて小さくなるため、20MB の `MAX_IMAGE_SIZE` チェックを容易に通過する。

2. FileTree の自動リフレッシュ（2秒間隔）でこのファイルがサイドバーに表示される。ファイルは通常のファイル（symlink ではない）であり、ワークスペース内に存在するため、Round 1/2 のセキュリティチェックをすべて通過する。

3. ユーザがこのファイルを FileTree で選択すると、`Preview::load()` が呼ばれる。`metadata.len()` チェック（L126）は圧縮後のディスクサイズを確認するだけなので通過する。

4. `image::ImageReader::open(path).decode()` が呼ばれ、PNG デコーダが画像全体をメモリ上に展開する。RGBA 8bit の場合、1 ピクセルあたり 4 バイト必要となるため、65535x65535 = 約 16GB のメモリが確保される。

5. ccmux プロセスが OOM (Out of Memory) で強制終了するか、システム全体のメモリが逼迫し、他のプロセス（実行中の Claude Code セッションを含む）にも影響が及ぶ。

現在のコードでは `image::ImageReader` にデコード制限（`limits()`）が設定されていないため、`image` クレート v0.25 のデフォルト制限のみが適用される。デフォルトでは allocation 制限が 512MB に設定されているが、これでもターミナルアプリケーションとしては過大であり、攻撃者はこの範囲内で十分な影響を与えることができる。また、`image` クレートのバージョンやビルド構成によってはデフォルト制限が異なる場合がある。

## 想定される影響

- **サービス停止（DoS）**: ccmux プロセスのメモリ消費が急増し、アプリケーションがクラッシュまたは応答不能になる。実行中の PTY セッション（Claude Code を含む）がすべて失われる。
- **システムリソースへの影響**: メモリ逼迫によりシステム全体のパフォーマンスが低下する可能性がある。特に swap が設定されていない環境では OOM Killer が他のプロセスも終了させる場合がある。
- **作業の喪失**: ccmux がクラッシュすると、保存されていない PTY セッションの出力やスクロールバック履歴が失われる。

## Defender への要求

### 修正方針

`image::ImageReader` にデコード制限を明示的に設定する。`image` クレート v0.25 は `ImageReader::set_limits()` メソッドを提供しており、メモリ割り当て上限とピクセル寸法上限を指定できる。

```rust
// 修正の方向性（具体的な実装は Defender に委ねる）
let mut reader = image::ImageReader::open(path)
    .and_then(|r| r.with_guessed_format())?;

let mut limits = image::io::Limits::default();
limits.max_alloc = Some(64 * 1024 * 1024);  // 展開後 64MB まで
limits.max_image_width = Some(8192);          // 幅 8192px まで
limits.max_image_height = Some(8192);         // 高さ 8192px まで
reader.limits(limits);

let dyn_img = reader.decode()?;
```

ターミナルプレビューの用途では、8192x8192 を超える画像をデコードする必要性はない。表示解像度はターミナルのセルサイズに制約されるため、大きな画像は結局リサイズされる（L141 の `picker.new_resize_protocol`）。デコード前にピクセル寸法を制限することで、不要なメモリ消費を防げる。

### 修正の妥当性を確認するための観点

- 通常サイズの画像（PNG, JPEG, GIF, BMP, WebP）が正常にプレビューできること。
- 8192x8192 以下の画像はプレビュー可能であること。
- 極端なピクセル寸法を持つ画像（例: 65535x65535）がデコード制限でブロックされること。
- ブロック時にパニックせず、適切なエラーメッセージが表示されること。
- `image` クレートの `Limits` API が現在の依存バージョン（v0.25）で利用可能であること。

## 過去ラウンドとの差分
- **直前ラウンドの修正内容**: Round 2 では `Preview::load()` に `canonicalize() + starts_with()` による境界チェックを追加し、symlink 経由のワークスペース外ファイル読み取り（TOCTOU）を防止した。
- **今回の指摘がそれとどう異なるか**: 新規。Round 2 の修正は「どのファイルを読むか」のアクセス制御に関するものだったが、今回は「許可されたファイルをどのようにデコードするか」のリソース制限に関するもの。攻撃対象のファイルはワークスペース内に正当に存在し、symlink でもなく、ディスクサイズも 20MB 以下であるため、Round 1/2 のすべてのセキュリティチェックを通過する。攻撃ベクタは画像デコンプレッション（圧縮比の悪用）であり、これまで指摘されていない新しいカテゴリの脆弱性である。
