# ADR-0001: スコープ・名称・依存方向

- 状態: 採用
- 日付: 2026-11-17
- 対象マイルストーン: M0

## 背景

`CLAUDE_HANDOFF.md`（以下「仕様」）は、Rust 製のテストオーケストレーター「DLE（仮称）」を
4 クレート構成で実装することを定めている。着手時に利用者から次の 2 点が指示された。

1. CLI コマンド名は `testry` とする。
2. コンテナでの実行を基本とし、ホスト側でランタイム等を実行しない（ADR-0002）。

## 決定

### 名称

仕様の `dle` は仮称であるため、すべて `testry` に統一する。

| 仕様 | 本実装 |
|---|---|
| CLI `dle` | `testry` |
| クレート `dle-domain` / `dle-application` / `dle-adapters` / `dle-cli` | `testry-domain` / `testry-application` / `testry-adapters` / `testry-cli` |
| 設定 `dle.toml` | `testry.toml` |
| 状態ディレクトリ `.dle/` | `.testry/` |
| 所有ラベル `dle.*`（M4） | `testry.*` |

### 依存方向

```text
testry-cli ─────────┬──> testry-application ──> testry-domain
                    └──> testry-adapters ─────> testry-application
                                              └> testry-domain
```

- `testry-domain`: 値オブジェクト、依存グラフ、選択ルール、plan、状態遷移。IO なし。
  直接依存は `blake3` / `globset` / `serde` / `serde_json` / `thiserror` のみ
  （いずれも純粋計算。glob 判定は選択ルールの一部なので domain に置く）。
- `testry-application`: ユースケースと IO ポート（trait）。domain のみに依存する。
- `testry-adapters`: ポートの実装（Git、Oxc、ファイル）。
- `testry-cli`: 引数、設定ロード、DI、表示、exit code。

ポートは仕様 5 章のとおり object-safe な boxed future で定義する。純粋な処理
（グラフ探索、選択、plan の正規化）は trait にせず関数のままにする。

### 規約の機械的な担保

- `crates/testry-cli/tests/architecture.rs` が `cargo metadata` の解決済みグラフから依存方向を検査する。
  domain から tokio / clap / oxc / rusqlite / 他の testry クレートへ到達したら失敗する。
  domain と application のソースに `std::fs` / `std::process` / `std::net` / `std::env` があれば失敗する。
- `#![forbid(unsafe_code)]` を全クレートに置き、同テストで存在を確認する。
- `unwrap` / `expect` / `panic!` / `todo!` は workspace lint で deny（テストコードは `clippy.toml` で除外）。
- テスト用 fake は feature `test-support` の内側に置き、出荷される依存から有効化されないことを検査する。

### v0.1 のスコープ

仕様 1 章の「v0.1 の成果物」「v0.1 の対象外」をそのまま採用する。特に次は実装しない。

- テスト成功結果のキャッシュ、AI によるテスト除外（設定で `true` にするとエラー）
- 関数・テストケース単位の解析、E2E の自動絞り込み
- Warm Pool、分散実行、常駐サービス、GUI、MCP サーバー

## 影響

- 仕様中のコマンド例・パス・ラベルは `dle` → `testry` に読み替える。
- 既存の空ディレクトリ `src/` は 4 クレート構成では使わない（削除はしていない）。
