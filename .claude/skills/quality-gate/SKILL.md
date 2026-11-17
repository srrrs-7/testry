---
name: quality-gate
description: testry の品質ゲート（fmt / clippy / test）や個別のテストをコンテナ内で実行する。コードを変更したあとの確認、「テストを実行して」「ci を通して」「このテストだけ走らせて」と言われたとき、完了を報告する前に使う。ホストで cargo を呼ばないための手順。
---

# 品質ゲートをコンテナ内で実行する

**ホストで `cargo` / `rustc` / `bun` / `node` を実行しない。** ホストで使うのは `make` と compose だけ。
この環境の compose は `docker-compose`（`docker compose` プラグインは壊れている。`make` は自動判定する）。

## 全体

```bash
make ci          # fmt-check → clippy -D warnings → test。ネットワーク遮断、--locked --offline
```

個別のゲート: `make fmt-check` / `make lint` / `make test`。整形を直すのは `make fmt`。

`make ci` は数分かかる（実ブラウザを起動するテストを含む）。出力が長いので、失敗箇所だけ知りたいときは
agent `gate-runner` に任せると、要約だけが返る。

## 一部だけ

ゲートと同じ条件（ネットワーク遮断）で、コマンドだけ差し替える:

```bash
# クレート単位
docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-adapters --locked --offline

# 結合テストのファイル単位（crates/<crate>/tests/<name>.rs）
docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-cli --locked --offline --test cli_run

# テスト名で絞る（部分一致）。単体テストは --lib
docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-adapters --locked --offline --test runner_contract -- playwright_focused
docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-domain --locked --offline --lib -- g18_

# 1 件だけ、出力つきで
docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-cli --locked --offline --test cli_run -- r17_a_second_run_in_the_same_workspace_is_refused --exact --nocapture
```

テストの場所の目安:

| 変更した場所 | まず走らせるもの |
|---|---|
| `testry-domain` | `-p testry-domain --lib` |
| `testry-application` | `-p testry-application`（`tests/planning.rs`、`tests/execution.rs` は fake ports） |
| `adapters/analysis`、`source` | `-p testry-adapters --lib`、`--test git_snapshot` |
| `adapters/runners`、`reports` | `-p testry-adapters --test runner_contract`（実ランナー・実ブラウザ） |
| `adapters/process` | `-p testry-adapters --test process` |
| `adapters/storage` | `-p testry-adapters --lib` |
| `testry-cli`、出力の形 | `-p testry-cli --test cli_plan` / `cli_run` / `cli_browser` / `schemas` |
| 版・Dockerfile・`compatibility.toml` | `-p testry-cli --test compatibility` |
| クレート間の依存 | `-p testry-cli --test architecture` |

## 失敗したとき

- `error: no matching package … offline` / `failed to download` → 依存が未取得。`make fetch`（ここだけネットワークを使う）のあと再実行。
- イメージが無い、ランナーやブラウザが見つからない → `make image`。
- `Cargo.lock` が合わない（`--locked`）→ 依存を変えたなら差分を確認してから `make fetch`。無断で版を上げない。
- ブラウザ系のテストだけ時間切れ → 他のコンテナと CPU を取り合っていないか確認して再実行する。
  繰り返し落ちるなら flaky として片付けず原因を調べる。
- 一部だけ通して完了にしない。**報告の前に `make ci` を最後まで通す。**

## 報告

- 実行したコマンド、exit code、通った件数を書く。失敗があれば、その出力を添える。
- 実行していないもの（別アーキテクチャ、実プロジェクトなど）は「未実行」と書く。通ったことにしない。
