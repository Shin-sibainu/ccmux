# Security Battle Log

ccmux 自己ペンテスト演習の戦績記録。Attacker / Defender エージェントによるラウンド制。

## 戦績一覧

| Round | 脆弱性 | 深刻度 | コミット | Attack | Defense |
|-------|--------|--------|----------|--------|---------|
| 1 | OSC 7 パストラバーサル | High | `9f4cce4` (既存) | [attack](round-1-attack.md) | [defense](round-1-defense.md) |
| 2 | Preview symlink TOCTOU | Medium | `4698acb` | [attack](round-2-attack.md) | [defense](round-2-defense.md) |
| 3 | 画像 Decompression Bomb | Medium | `8059696` | [attack](round-3-attack.md) | [defense](round-3-defense.md) |
| 4 | npm version UI スプーフィング | Medium | `32b3a8a` | [attack](round-4-attack.md) | [defense](round-4-defense.md) |
| 5 | Bracketed Paste Injection | High | `f12dfc2` | [attack](round-5-attack.md) | [defense](round-5-defense.md) |

## 統計

- 総ラウンド: 5
- High: 2件 (Round 1, 5)
- Medium: 3件 (Round 2, 3, 4)
- テスト追加: +4件 (30 → 34)

## 攻撃ベクタの分類

| カテゴリ | Round | 経路 |
|----------|-------|------|
| エスケープシーケンス注入 | 1, 5 | PTY 出力 (OSC 7), クリップボード (Bracketed Paste) |
| ファイルシステム操作 | 2, 3 | symlink TOCTOU, 画像デコード |
| 外部入力 (ネットワーク) | 4 | npm レジストリレスポンス |
