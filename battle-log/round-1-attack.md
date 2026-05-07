# Round 1 - Attack Report

## 脆弱性
- **種別**: Path Traversal via OSC 7 Escape Sequence (Unsanitized PTY Output Injection)
- **OWASP分類**: A03:2021 – Injection (エスケープシーケンスインジェクション / 信頼境界の欠如)
- **深刻度**: High

## 該当箇所

### 発生源: PTY 出力の OSC 7 パース
- ファイル: `src/pane.rs`
- 行番号: L295-L344
- 該当コード(抜粋):

```rust
fn extract_osc7(data: &[u8]) -> Option<PathBuf> {
    let s = std::str::from_utf8(data).ok()?;
    let marker = "\x1b]7;";
    let start = s.find(marker)?;
    let rest = &s[start + marker.len()..];
    let end = rest.find('\x07').or_else(|| rest.find("\x1b\\"));
    let uri = &rest[..end?];

    if let Some(path_str) = uri.strip_prefix("file://") {
        // ... hostname スキップ → PathBuf を生成して返す
        return Some(PathBuf::from(path));
    }
    None
}
```

### 受け取り側: 検証ロジック
- ファイル: `src/app.rs`
- 行番号: L1630-L1656
- 該当コード(抜粋):

```rust
AppEvent::CwdChanged(pane_id, new_cwd) => {
    // Security: resolve symlinks and relative components.
    // Reject paths that don't resolve to a real directory
    let new_cwd = match new_cwd.canonicalize() {
        Ok(p) if p.is_dir() => p,
        _ => continue,
    };
    // ... FileTree のルートを new_cwd で置き換える
    ws.file_tree = FileTree::new(new_cwd.clone());
    ws.cwd = new_cwd;
    // ...
}
```

## 攻撃シナリオ(概念説明のみ)

OSC 7 (`\x1b]7;file://HOST/PATH\x07`) はシェルが現在ディレクトリを端末エミュレータに通知するための標準的なエスケープシーケンスである。ccmux はこれを PTY 出力のバイトストリームから検出し、`CwdChanged` イベントとして処理する。

問題は、このシーケンスを出力できるのが「シェル」に限らない点にある。pane 内で実行中のプログラム（ビルドツール、パッケージマネージャ、悪意ある npm/cargo スクリプト、あるいはソーシャルエンジニアリングで実行させたコマンド）が任意の OSC 7 シーケンスを stdout に書き出すだけで、ccmux の FileTree ルートを攻撃者が指定した任意のディレクトリに変更できる。

`canonicalize()` + `is_dir()` の検証は「そのパスがファイルシステム上に存在する実ディレクトリであること」しか保証しない。ユーザーが ccmux を起動した元の作業ディレクトリとの一致、あるいは初期 cwd のサブディレクトリかどうか、という関係性は検証されていない。

例えば、pane 内のプログラムが `\x1b]7;file:///etc\x07` を出力すれば、FileTree は `/etc` 以下の全ファイル名を UI に列挙し始める。`\x1b]7;file:///home/<ユーザー名>/.ssh\x07` であれば SSH 関連のファイル名が可視化される。さらに、FileTree 上でファイルを選択すると Preview に内容が表示されるため、**ファイル名の列挙にとどまらず内容の読み取りまで誘導できる**。

## 想定される影響

- **機密ファイル名の列挙**: `/etc`, `~/.ssh`, `~/.aws`, `~/.claude` など、作業ディレクトリ外の任意ディレクトリのファイル一覧が UI に表示される。
- **機密ファイル内容の開示**: FileTree 上でファイルを選択すると Preview に内容が読み込まれる (`preview.rs` の `Preview::load`)。テキスト形式の秘密鍵・設定ファイルが 500 行分まで画面に表示される。
- **信頼の損壊**: ユーザーは FileTree が現在のプロジェクトディレクトリを示していると信じるが、実際には別のパスを指している状態になる。
- **前提条件の低さ**: 攻撃者がシェルセッション内で任意コードを実行できる状況（= npm run, cargo build, make 等でスクリプトが走る状況）があれば成立する。これはローカル開発環境では珍しくない条件である。

## Defender への要求

### 修正方針

1. **CWD を初期ルート配下に限定する**  
   `AppEvent::CwdChanged` の受け取り側 (`app.rs` L1634) で、`canonicalize()` 後のパスが「そのペインが属するワークスペースの初期 cwd」のサブパスであることを確認する。

   ```rust
   let canonical = match new_cwd.canonicalize() {
       Ok(p) if p.is_dir() => p,
       _ => continue,
   };
   // 追加: 初期ルートの外を拒否
   let root = ws.cwd.canonicalize().unwrap_or_else(|_| ws.cwd.clone());
   if !canonical.starts_with(&root) {
       continue; // サイドチェイン外のパス変更を無視
   }
   ```

   ただし、ユーザーが意図的に `cd /other/project` する正当なユースケースを壊す可能性があるため、UX との兼ね合いを要検討。その場合は下記の代替手段も組み合わせる。

2. **OSC 7 の受け入れを既知プロセスのみに限定する**  
   `pane.rs` の `pty_reader_thread` はすべての PTY バイトストリームから OSC 7 を検出する。バイトストリーム中の OSC 7 が「直接 shell が生成した」かどうかは区別できないため、根本的な信頼境界はアプリケーション側では引けない。ただし、シーケンスを注入した主体を特定する追加ヒューリスティック（PROMPT_COMMAND のみで送出されることが期待されるため、OSC 7 の受信頻度や前後のバイト列から判断する）を導入することは可能。

3. **FileTree ルート変更を UI で明示する**  
   最低限の対処として、OSC 7 によって FileTree のルートが変わった場合にステータスバーや警告 UI でユーザーに通知し、意図的な変更かどうかを確認できるようにする。

### 修正の妥当性を確認するための観点

- 修正後、`cd /etc` をシェルで実行した場合に FileTree が `/etc` を表示するか（正常な cwd 追跡の動作確認）。
- プログラム（例: シェルスクリプト内で `printf '\033]7;file:///etc\007'` を実行）が OSC 7 を出力した場合、FileTree のルートが変わらないこと（または警告が表示されること）。
- サブディレクトリへの移動（`cd src/`）が正しく FileTree に反映されること。
- Windows/MSYS2 のパス変換パス (`L328-338`) でも上記バリデーションが適用されること。

## 過去ラウンドとの差分
- Round 1 のため該当なし
