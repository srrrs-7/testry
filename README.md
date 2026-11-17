# testry

変更に必要なテストを **計画（plan）し、理由を説明（explain）し、実行する** ための Rust 製テストオーケストレーター。
既存のランナー（Bun test / Vitest / Storybook の Vitest 連携 / Playwright）を置き換えず、その外側で
「何を走らせるか」「どの環境で走らせるか」「結果をどう統合するか」を扱う。

設計の正本は [`CLAUDE_HANDOFF.md`](CLAUDE_HANDOFF.md)（仕様では仮称 `dle`。本実装では `testry`）。

## 現在の状態（2026-11-17）

| マイルストーン | 内容 | 状態 |
|---|---|---|
| M0 | 4 クレート構成、CLI 引数、設定、domain 型、型付きエラー、ポート、fake、アーキテクチャテスト | 完了 |
| M1 | Git スナップショット、TS/JS 依存グラフ、選択ルール、決定的な plan、`plan` / `explain` / `doctor` | 完了 |
| M2 | Bun / Vitest / command の実行、プロセス制御、資源スケジューラー、結果統合、run の保存、workspace trust | 完了 |
| M3 | Storybook（Vitest addon）/ Playwright のアダプター、project の識別、グループ外プロセスの回収 | 完了 |
| M4 | Dev Container CLI による使い捨て環境 | **実装しない**（利用者の決定。[ADR-0006](docs/adr/0006-no-dev-container-provider.md)） |
| M5 | 実行履歴（SQLite）、`runs list`、gc、JSON Schema、ベンチマーク | 完了 |

**動くコマンドは `plan` / `explain` / `doctor` / `trust` / `test` / `verify` / `runs show` / `runs list` / `gc`**。
実行できるランナーは Bun（`bun test`）・Vitest・Storybook（Vitest addon 経由）・Playwright・任意コマンド（成否のみ）で、
環境は `host` provider（testry と同じコンテナ内の子プロセス）だけ。testry は Dev Container を作らない。
`--environment` で Dev Container の環境を指定した要求は exit 2 で止まり、成功は返さない。
受け入れ条件ごとの検証状況は [`docs/acceptance.md`](docs/acceptance.md)。

## 使い方（開発用コンテナ内で実行する）

ホストに必要なのはコンテナエンジンと compose、`make` だけ。Rust などのツールチェーンは
すべてコンテナ内にある（[ADR-0002](docs/adr/0002-container-first-execution.md)）。

```bash
make image    # 開発イメージをビルド（Rust 1.100.0 固定）
make fetch    # 依存を取得（ここだけネットワークを使う）
make ci       # 品質ゲート: fmt / clippy -D warnings / test（ネットワーク遮断）
make bench    # ベンチマーク（release ビルド、ネットワーク遮断。結果は docs/benchmarks/）

make testry ARGS="--help"
make shell    # コンテナに入る
```

対象リポジトリで使う場合は、そのリポジトリを `/workspace` にマウントしたコンテナ内で実行する。
リポジトリのルート（Git 作業ツリーの最上位）に `testry.toml` を置く。
記入例: [`crates/testry-cli/tests/fixtures/sample.testry.toml`](crates/testry-cli/tests/fixtures/sample.testry.toml)。

```bash
# 未コミットの変更（staged / unstaged / 未追跡）に必要なテストを計画する。既定の base は HEAD
testry plan

# ブランチ全体を対象にする（merge-base を使う。fetch はしない）
testry plan --base origin/main --format json

# 登録された全 suite を全体実行する計画（差分も base も不要）
testry plan --mode full

# 保存された plan について、suite の選択理由を表示する
testry explain --plan .testry/plans/blake3-<hex>.json --suite api-unit

# 読み取り専用の診断
testry doctor
```

`plan` は Git とファイル読み取りだけで動く。`node_modules` は不要で、ランナー設定もテストコードも実行しない。
出力例:

```text
Plan blake3:9e90…53c8
  mode: affected
  assurance: advisory (narrowed or scope-limited; not a quality gate)
  baseline: 9b7d2749c362c0003e4462fa39bdf2dc670f6a98
  changed paths: 1
    packages/shared/src/index.ts

Suite api-unit (bun)
  runs: 1 test file(s)
    apps/api/src/user.test.ts
  why:
    - depends on a changed file: apps/api/src/user.test.ts -> apps/api/src/user.ts -> packages/shared/src/index.ts

Suite e2e (playwright)
  runs: all
  why:
    - the suite has selection = "whole_suite" and a relevant change exists
```

### 実行する

`test` / `verify` は `testry.toml` に書かれたコマンドを実行するので、**最初にその設定を承認する**必要がある。
承認は設定の指紋に結びつき、設定を変えると無効になる（承認の保存先はリポジトリの外）。

```bash
# 実行されるコマンドを確認してから承認する
testry trust
testry trust --approve

# 未コミットの変更に必要なテストだけを実行する（advisory。品質ゲートではない）
testry test
testry test --suite api-unit

# 登録された全 suite を全体実行する（品質ゲート）
testry verify

# 全 suite を 1 回実行しつつ、affected なら漏らしていた失敗を記録する
testry verify --mode shadow --base origin/main

# 結果を機械可読で受け取る（stdout は JSON 1 個）/ イベントを逐次受け取る
testry verify --format json
testry verify --format jsonl

# 過去の run を新しい順に一覧する / 保存された run を読む
testry runs list
testry runs show <run-id> --format json

# 保持期間（storage.history_retention_days、既定 14 日）を過ぎた run と plan を確認して削除する。
# 中断された run が残したプロセスも表示・停止する
testry gc --dry-run
testry gc --apply
```

CI では、信頼できるジョブ定義から `TESTRY_TRUSTED_CONFIG_DIGEST=blake3:<hex>`（`testry trust --format json` の
`config_digest`）を渡す。PR が `testry.toml` を変えると一致しなくなり、実行されない。

run ごとの成果物は `.testry/runs/<run-id>/` に残る（Git からは無視される。権限は 0700 / 0600）:

```text
run.json  plan.json  events.jsonl  result.json  environment.json  junit.xml
tasks/<suite>/{stdout.log, stderr.log, report.xml | report.json}
```

suite の書き方の注意:

- `command` はランナーの起動部分だけを書く（例 `["bun", "test"]`、`["node", "node_modules/vitest/vitest.mjs", "run"]`、
  `["node", "node_modules/@playwright/test/cli.js", "test"]`）。レポート・出力先・並列数・絞り込みの引数は testry が付ける。
  同じ意味のフラグや範囲を狭めるフラグ（`--grep`、`--shard`、`--no-deps` など）を書くと設定エラーになり、何も実行しない。
- `bunx` / `npx` のように実行時に取得しうるランチャーは使えない。
- **Storybook** は `runner = "storybook_vitest"` と `project`（Vitest の project 名。必須）を書く。その project 全体が走る。
- **Playwright** は常に suite 全体が走る。project の依存関係・setup project・`webServer`・retries は Playwright の設定のまま。
  `project` を書くとその project（と依存先）だけを実行する。成果物（trace など）は run のディレクトリに出力される。
- Storybook と Playwright は `selection = "whole_suite"` のみ。差分があれば、差分の場所にかかわらず全体を実行する。
  ブラウザを使う suite を同時に走らせたくない場合は、同じ排他キー（`exclusive = ["browser-budget"]`）を付ける。
- ランナーに渡る環境変数は `PATH` `HOME` `TMPDIR` `LANG` `LC_ALL` `LC_CTYPE` `TZ` `TERM` `CI` だけ。
  加えて testry が `DO_NOT_TRACK=1` を渡す（Bun には `CI=true`、Storybook には `STORYBOOK_DISABLE_TELEMETRY=1` も）。
- 既定では skip / todo / expected failure（`test.fail()`）と flaky（retry 後に成功）は失敗になる。
  許可するものは suite の `known_skips` に完全一致で書く（Playwright はブラウザの project も指定できる）。
- ランナーが作業ツリーに書くキャッシュや成果物（Vite の `cacheDir` など）は `.gitignore` に入れておく。
  無視されていない新規ファイルができると「実行中に入力が変わった」として結果が無効になる。

## 知っておくべき意味

- **`affected` は advisory**。ローカルの反復用で、単独では品質ゲートにならない。CI では `test` / `verify` の
  `affected` と `--suite` を拒否する。
- **`full` は「`testry.toml` に登録された suite すべて」**。リポジトリ内の未登録テストを発見・保証するものではない。
- **不確実なら広げる**。非 literal の動的 import、未解決の import、パース失敗、lockfile や設定の変更、
  どのテストからも到達しない変更があると、全 suite の全体実行に拡大する。
- **対象 0 件は成功ではない**。`no_work`（exit 4）として区別する。
- **レポートが根拠**。ランナーが exit 0 でも、レポートが無い・壊れている・実行予定のファイルが載っていない場合は
  成功にしない。command アダプターは成否だけを扱い、テスト件数は `unknown` と表示する。
- **検証済みでない版のランナーではファイルを絞り込まない**。suite 全体を実行する（`compatibility.toml`）。
- **実行中に入力が変わったら結果は無効**（`invalidated`、exit 3）。
- **`full_suite` と表示されるのは**、登録された全 suite を全体実行し、入力が変わらず、後始末が完了したときだけ。
- **`inputs` / `global_inputs` は利用者が宣言する**。テストが実行時に読むファイルや環境変数は import グラフでは
  分からない。共有 setup、モック共通ハンドラー、スキーマなどは `global_inputs` に書く。

### exit code

| code | 意味 |
|---|---|
| 0 | 指定 scope / mode の検証成功、または読み取り専用操作の成功 |
| 1 | テスト失敗・ポリシー違反（想定外の skip、flaky） |
| 2 | 設定不正、非対応能力、base 不足、選択条件不正、未承認の設定、未実装コマンド、コンテナ外での実行 |
| 3 | 実行基盤の異常（timeout、レポート欠落、起動失敗）、入力の変化、保存の失敗、同じ workspace で実行中 |
| 4 | 検証対象なし（成功とは別） |
| 130 | ユーザーキャンセル（SIGINT / SIGTERM。後始末をしてから終了） |

## 既知の制約（v0.1 / 現時点）

- 検証済みの版は Bun 1.4.4 / Vitest 5.1.0 / Storybook 10.6.7 / Playwright 1.65.1（Chromium のみ）。
  Bun / Vitest は検証済み以外の版では絞り込まず suite 全体を実行する。
- 実行環境は `host` provider（testry と同じコンテナ内）だけで、**suite 間の隔離は無い**。資源の予約は
  スケジューリング上の予算であり OS による制限ではない。ブラウザや開発サーバーのようにプロセスグループの外へ出た
  プロセスは環境変数の目印で探して止めるが、環境を消して起動されたプロセスは追跡できない。
- Storybook の `tags: ["!test"]` で除外された story、Playwright の設定ファイル内の `grep` による絞り込みは、
  レポートから判別できず検出できない。Playwright の `shard` / `grepInvert` / `maxFailures` は検出して結果に明示する。
- story ファイルが読み込めないとき Vitest はレポートを書かないので、テスト失敗（exit 1）ではなく基盤異常（exit 3）になる。
- コンテナの外では既定で実行しない（`TESTRY_ALLOW_EXECUTION_OUTSIDE_CONTAINER=1` で許可できるが未検証）。
- suite へ任意の環境変数を渡す手段がまだ無い（許可リストの変数のみ）。API キーなどが必要なテストは実行できない。
- Bun / Vitest の設定ファイル（`bunfig.toml`、`vitest.config.*`）による絞り込みは検査しない。
- Bun の per-test `{ retry }` はレポートに現れないため flaky として検出できない。
- リゾルバのプロファイル（bun / vite / node）と実ランナーの解決規則の一致は、fixture の範囲でしか確認していない
  （[`docs/compatibility.md`](docs/compatibility.md)）。
- 古い run は自動では消えない。`testry gc --apply` を実行したときだけ、保持期間を過ぎたものが削除される。
  `gc` が消すのは testry のものだと証明できる run ディレクトリと plan ファイルだけで、それ以外は報告するだけ。
- testry 自身が強制終了された場合、起動済みのランナーは動き続ける。その run は `runs list` に INTERRUPTED と表示され、
  残ったプロセスは `testry gc --dry-run` で確認、`--apply` で停止できる（testry の目印を環境変数に持つプロセスに限る）。
- 解析結果のディスクキャッシュは実装していない（計測上は不要だった。[`docs/benchmarks.md`](docs/benchmarks.md)）。
- CI のゲート規則（affected / `--suite` の拒否）がかかるのは `test` / `verify` だけで、`plan` にはかからない。
- グラフ診断が 1 件でもあると affected は全 suite に拡大する（仕様どおり）。`virtual:` モジュール、
  `import.meta.glob`、外部パッケージを `extends` する `tsconfig.json`、ビルド出力（`dist/`）を指す
  ワークスペースパッケージがあるリポジトリでは、affected は常に全体実行になる。
- Git ignore された input（`.env` など）は指紋には入るが、基準が無いので「変更」として検出できない。
- symlink と submodule は辿らない。それらを経由する import は診断になり、全体実行へ拡大する。
- `vite.config` の `resolve.alias` は読まない（設定を実行しないため）。`tsconfig.json` の `paths` は解決する。
- `/// <reference path>`、`new URL("./x", import.meta.url)`、`fs.readFile` などの実行時ファイル参照は辺にならない。
- 検証済みの実行環境は linux-aarch64 コンテナのみ（[`docs/compatibility.md`](docs/compatibility.md)）。
- 性能の計測は合成リポジトリによるものだけで、実プロジェクトでは未計測（[`docs/benchmarks.md`](docs/benchmarks.md)）。

## ドキュメント

- [`docs/architecture.md`](docs/architecture.md) — 構成、`plan` と `test` / `verify` の流れ、結果の決まり方
- [`docs/acceptance.md`](docs/acceptance.md) — 受け入れ条件とテストの対応、検証状況
- [`docs/compatibility.md`](docs/compatibility.md) / [`compatibility.toml`](compatibility.toml) — 検証済みの版と対応機能
- [`docs/benchmarks.md`](docs/benchmarks.md) — 計測方法と結果（生データは `docs/benchmarks/`）
- [`schemas/`](schemas/) — 出力と設定の JSON Schema（`--format json` の各文書、`events.jsonl` の 1 行、`testry.toml`）
- [`docs/adr/`](docs/adr/) — 設計判断の記録（0001 範囲と依存方向 / 0002 コンテナ優先 / 0003 計画 / 0004 実行 /
  0005 Storybook・Playwright / 0006 Dev Container を実装しない / 0007 履歴・gc・スキーマ）
