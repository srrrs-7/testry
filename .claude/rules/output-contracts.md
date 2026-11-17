---
paths:
  - "schemas/**"
  - "crates/testry-cli/src/**"
  - "crates/testry-cli/tests/schemas.rs"
  - "crates/testry-cli/tests/fixtures/**"
  - "crates/testry-domain/src/plan.rs"
  - "crates/testry-domain/src/results.rs"
  - "crates/testry-application/src/results.rs"
  - "crates/testry-adapters/src/storage/run_files.rs"
  - "crates/testry-adapters/src/reports/junit.rs"
---

# 出力と設定の契約

## JSON Schema

- `schemas/*.schema.json`（draft 2020-12）は、`--format json` の各文書、`events.jsonl` の 1 行、`testry.toml` の形を定める。
- **出力や設定の形を変えたら、同じ変更で `schemas/` を更新する。**
  `crates/testry-cli/tests/schemas.rs` が実バイナリの出力をスキーマで検証し、壊した文書が拒否されることも検査する。
- スキーマを緩めて（`additionalProperties` を開ける、必須を外すなど）テストを通さない。
  新しい文書を足したら、合格する例と拒否される例の両方をテストに入れる。
- 設定の項目を足したら `crates/testry-cli/tests/fixtures/sample.testry.toml` と README の説明も合わせる。

## 標準出力

- `--format json` の stdout は JSON 値ちょうど 1 個。ログや進捗は stderr へ。
- `--format jsonl` は `seq` が 1 ずつ増えるイベント列で、`events.jsonl` と同じ行。
- 改変された plan、他のバージョンの plan は拒否する（`explain` を含む）。外部から渡された plan JSON を実行しない
  （`test` / `verify` は plan を内部で作り直す）。

## exit code

| code | 意味 |
|---|---|
| 0 | 指定 scope / mode の検証成功、または読み取り専用操作の成功 |
| 1 | テスト失敗・ポリシー違反（想定外の skip、flaky） |
| 2 | 設定不正、非対応能力、base 不足、未承認の設定、未実装コマンド、コンテナ外での実行 |
| 3 | 実行基盤の異常（timeout、レポート欠落、起動失敗）、入力の変化、保存の失敗、同じ workspace で実行中 |
| 4 | 検証対象なし（`no_work`。成功とは別） |
| 130 | ユーザーキャンセル（後始末をしてから終了） |

- **未実装・非対応の機能が 0 を返す経路を作らない**（`CliError::unsupported` → exit 2）。
- `no_work` を 0 にまとめない。`invalidated`（実行中に入力が変わった）を 1 や 0 にしない。

## 言葉の意味を変えない

- `affected` は advisory。CI では `test` / `verify` の `affected` と `--suite` を拒否する（`plan` にはかからない）。
- `full` は「`testry.toml` に登録された suite すべて」。未登録のテストの発見・保証ではない。
- `full_suite` の assurance を名乗れるのは、登録された全 suite を全体実行し、入力が変わらず、cleanup が完了したときだけ。
