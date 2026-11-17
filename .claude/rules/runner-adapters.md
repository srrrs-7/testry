---
paths:
  - "crates/testry-adapters/src/runners/**"
  - "crates/testry-adapters/src/reports/**"
  - "crates/testry-adapters/tests/runner_contract.rs"
  - "crates/testry-cli/tests/compatibility.rs"
  - "crates/testry-cli/tests/cli_browser.rs"
  - "compatibility.toml"
  - "tests/fixtures/runners/**"
  - "tests/fixtures/runner-reports/**"
---

# ランナーのアダプター

判断の根拠は `docs/adr/0004-execution-rules.md`（実行全般）と `docs/adr/0005-browser-suites.md`（Storybook / Playwright）。

## 守る不変条件

- **広げるのはよいが、狭めない。** 検証済みでない版ではファイル選択をやめて suite 全体を実行する
  （結果に `selection_widened`）。版が違うことを理由に実行を拒否しない。
- **レポートが根拠。** exit 0 でもレポートが無い・壊れている・大きすぎる・実行予定のファイルが載っていない場合は
  合格にしない（infrastructure_error）。レポートと exit code が食い違えば failed とし、anomaly に記録する。
- **テストが 1 件も実行されなかった suite を passed にしない。**
- **予約済みフラグは設定エラー。** suite の `command` に、testry が付ける引数と同じ意味のもの、または範囲を狭めるもの
  （`--grep`、`--shard`、`--no-deps`、`--retry`、`-j4` のような短縮形の連結も含む）があれば `prepare` で拒否する。
  `prepare` は最初の suite を始める前に全 suite 分が呼ばれる（preflight）ので、副作用を持たせない。
- **ランナー自身の設定による絞り込み**（Playwright の shard / grepInvert / maxFailures / forbidOnly=false）は
  `scope_limited_by_runner_config` として結果に残し、`full_suite` を名乗らせない。
- skip / todo / expected failure / flaky は passed に合算しない。区別したまま application の policy に渡す。
- 同じファイルが複数の project で走る場合、project を case の識別に含める（P11）。

## ランナーごとの実行単位

| ランナー | 単位 | testry が付けるもの |
|---|---|---|
| `bun` | 検証済みの版ではファイル単位 | `./<file>`、`--reporter=junit --reporter-outfile=…`、`CI=true` |
| `vitest` | 検証済みの版ではファイル単位（上位集合） | `--project`、`--reporter=json --outputFile=…`、`--maxWorkers=N`、`--allowOnly=false` |
| `storybook_vitest` | 常に Vitest の 1 project 全体（`project` 必須） | Vitest と同じ、`STORYBOOK_DISABLE_TELEMETRY=1` |
| `playwright` | 常に suite 全体（setup project と依存を維持） | `--reporter=json`、`--output=<run 内>`、`--workers=N`、`--forbid-only`、`PLAYWRIGHT_JSON_OUTPUT_FILE` |
| `command` | コマンド全体。成否のみ、件数は `unknown` | なし |

Vitest と Storybook は `runners/vitest.rs` の `prepare_run` / `normalize_run` / 版の確認を共有している。片方だけ変えない。

## 変更したら必ず通すもの

- `crates/testry-adapters/tests/runner_contract.rs` — 実物のランナー（とブラウザ）を起動する契約テスト。
  アダプターやレポート解析を変えたら、単体テストだけで済ませない。
- `crates/testry-cli/tests/compatibility.rs` — `VERIFIED_VERSIONS`、`compatibility.toml`、`Dockerfile`、
  `tests/fixtures/runners/package.json` の版が一致することを検査する。版はこの 4 か所を同じ変更で更新する
  （手順は skill `runner-version`）。
- レポートの記録 fixture（`tests/fixtures/runner-reports/<runner>-<version>/`）は実物の出力を保存したもの。
  手で書き換えて期待値に合わせない。形が変わったなら、その版で取り直す。

## 検出できないと分かっているもの

追加で対応する場合を除き、「検出できる」と書かない（`docs/compatibility.md` の「確認できていないこと」）。

- Bun の per-test `{ retry }`（JUnit XML に現れない）
- Storybook の `tags: ["!test"]` による story の除外
- Playwright の設定ファイル内の `grep`、project ごとの `testIgnore`
