---
paths:
  - "Dockerfile"
  - "compose.yml"
  - "compose.test.yml"
  - "Makefile"
  - ".dockerignore"
  - "rust-toolchain.toml"
  - "scripts/**"
  - "tests/fixtures/runners/**"
---

# 開発用コンテナ

根拠は `docs/adr/0002-container-first-execution.md`。

- ホストにあるのはコンテナエンジン、compose、`make` だけという前提を崩さない。
  ホストへツールチェーンを入れる手順や、ホストで動くスクリプトを足さない。
- **ゲート（`compose.test.yml`）は `network_mode: none` と `--locked --offline` を外さない。**
  ネットワークを使ってよいのは `make image`（イメージのビルド）と `make fetch` / `make lock` だけ。
- 版は固定する（Rust は `rust-toolchain.toml` と `Dockerfile`、Bun / Node / ランナーは `Dockerfile` と
  `tests/fixtures/runners/package.json` + `bun.lock`）。`latest` や範囲指定を使わない。
  変えたら `compatibility.toml` と合わせる（`crates/testry-cli/tests/compatibility.rs` が照合）。
- ランナー契約テスト用の依存は、イメージのビルド時に `/opt/testry/fixtures/runners/node_modules` へ入る
  （`TESTRY_RUNNER_FIXTURE_MODULES`）。テストの実行時に取得させない。
  `.dockerignore` はその manifest 2 つだけをビルドコンテキストに入れている。
- 開発イメージには Chromium（Playwright の headless shell）が入っていて約 2.7 GB ある。
  Storybook / Playwright を手で動かすときは `STORYBOOK_DISABLE_TELEMETRY=1` を付ける。
- `make` は compose を自動判定する（`docker compose` が使えなければ `docker-compose`）。
  別のプロジェクト名で試すときは `make COMPOSE="docker-compose -p <name>" …`。
- `make clean` は cargo のボリュームごと消す（次回は `make fetch` からやり直し）。利用者に断ってから実行する。
- `scripts/` のスクリプトはコンテナ内の Bun で動かす（`make bench`）。ホストの `node` / `python3` で動かさない。
- testry の承認状態はボリューム `testry-state`（`/home/dev/.local/state`）にあり、作業ツリーの外にある。
  これを作業ツリー内へ移さない。
