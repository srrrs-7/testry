# 互換性表

実際に動かして確認した組み合わせだけを「検証済み」とする。確認日: 2026-11-17。

## testry のビルド・実行環境

| 項目 | 版 | 状態 |
|---|---|---|
| Rust | 1.100.0（`rust:1.100.0-slim-trixie`、`rust-toolchain.toml` で固定） | 検証済み |
| OS / CPU | Debian 13 コンテナ / linux-aarch64（Colima, macOS Virtualization.Framework） | 検証済み |
| OS / CPU | linux-x86_64 | 未検証 |
| OS / CPU | macOS ネイティブ | 未検証（ADR-0002 により対象外の運用） |
| Git | 2.47.3（コンテナ内） | 検証済み（スナップショットの結合テスト） |
| コンテナエンジン | Docker Engine 29.2.1 / docker-compose 5.7.1（classic builder） | 検証済み（開発環境として） |
| Bun / Node（開発イメージ内） | 1.4.4 / 26.13.1 | 検証済み（ランナー契約テスト・E2E の実行に使用） |

## 解析・保存ライブラリ

| crate | 版 | 用途 |
|---|---|---|
| oxc_parser / oxc_ast / oxc_ast_visit / oxc_allocator / oxc_span | 0.159.0 | TS/JS の構文解析 |
| oxc_resolver | 11.24.3 | import の解決 |
| rusqlite（libsqlite3-sys 0.40.0 の同梱 SQLite をビルド） | 0.43.0 | 実行履歴 `state.sqlite3`。システムの SQLite は使わない |
| jsonschema | 0.61.2 | テスト専用（`schemas/` と実出力の照合）。ネットワーク解決・TLS の機能は無効 |

履歴のスキーマ版は 1。より新しい版で書かれた `state.sqlite3` は開かず、変更もしない。

API は採用版のソースで確認した（`ExportFromDeclaration`、`ParserReturn::fatal_error` / `diagnostics`、
`FileSystem` trait、`TsconfigDiscovery::Auto` + `resolve_file`）。版を上げるときは
`crates/testry-adapters/src/analysis/` のテストを再実行し、`ANALYSIS_VERSION` を更新する。

## ランナー・実行環境

機械可読な正本は [`compatibility.toml`](../compatibility.toml)。この表と `compatibility.toml` が、アダプターの
`VERIFIED_VERSIONS`・Dockerfile・fixture の `package.json` と食い違うと
`crates/testry-cli/tests/compatibility.rs` が失敗する。

| 対象 | 版 | 状態 | 対応機能 |
|---|---|---|---|
| Bun（`bun test`） | 1.4.4 | 検証済み | 全体実行、ファイル選択（`./path`）、JUnit XML、ファイル帰属。worker 制御なし |
| Vitest | 5.1.0（Node 26.13.1 上） | 検証済み | 全体実行、ファイル選択（指定を含む上位集合）、JSON レポート、ファイル帰属、`--maxWorkers`、project 指定、flaky 検出 |
| Storybook（Vitest addon） | Storybook 10.6.7 / `@storybook/addon-vitest` 10.6.7 / `@storybook/html-vite` 10.6.7、Vitest 5.1.0 のブラウザモード（`@vitest/browser-playwright` 5.1.0、Vite 8.3.8） | 検証済み | Vitest の 1 project 全体を実行。JSON レポート、story ファイル単位の帰属、`--maxWorkers`。ファイル選択なし |
| Playwright | 1.65.1（Chromium headless shell） | 検証済み | suite 全体を実行。JSON レポート、ファイルと project の帰属、`--workers`、setup project / project 依存の維持、flaky・expected failure の区別。ファイル選択なし |
| command アダプター | — | 検証済み | 成功 / 失敗のみ。テスト件数・ファイルは `unknown` |
| `host` provider | — | 検証済み | testry と同じコンテナ内の子プロセス。隔離なし |
| Dev Container CLI | — | **実装しない**（ADR-0006） | 該当する環境を `--environment` で指定すると exit 2。testry は Docker に接続しない |

「検証済み」は `crates/testry-adapters/tests/runner_contract.rs` で実物を起動して確認した範囲を指す
（ファイル指定の意味、レポートの形、exit code、`.only`、retry、import エラー、テスト 0 件、setup project、
タイムアウト時のブラウザと `webServer` の後始末）。ブラウザは Chromium だけで確認しており、
Firefox / WebKit、React などの他の Storybook フレームワークは未確認。

### 検証済み以外の版の扱い

- Bun / Vitest の版が上の表と**完全一致しない**場合、ファイル選択を使わず suite 全体を実行する
  （結果に `selection_widened` が残る）。レポートの解析は試み、読めなければ infrastructure_error になる。
- Storybook / Playwright はもともと全体実行なので、版が違っても実行範囲は変わらない。レポートの形が
  変わっていれば解析に失敗し、infrastructure_error になる（合格にはならない）。
- Storybook 本体の版は実行時に確認しない（確認できるのは Vitest の版）。
- 版は run のたびに実コマンド（`bun --version` など）で確認する。`doctor` は suite のコマンドを実行しないので、
  インストール済みの版を表示しない（`runner_versions: not_checked`）。
- 版を追加するときは、その版でイメージを作り直して契約テストを通し、`VERIFIED_VERSIONS` と
  `compatibility.toml` を同時に更新する。利用者の lockfile を testry が書き換えることはない。

### 確認できていないこと

- リゾルバのプロファイル（bun / vite / node）と実ランナーの解決規則の一致は、fixture の範囲
  （拡張子を省いた相対 import と候補の優先順、`./x.js` → `x.ts` の対応、ディレクトリの index）でのみ確認した。
  ランナー間で優先順が違う箇所は全候補を依存に入れている。`package.json` の `exports` 条件分岐、
  ワークスペースパッケージ、`tsconfig` の `paths`、`vite.config` の alias は実ランナーとの突き合わせをしていない。
- Bun の per-test `{ retry }`（レポートに現れない）、ワークスペース設定ファイルによる project の自動分割。
- Storybook の `tags` による story の除外（レポートに現れない）、Playwright の設定ファイル内の `grep` や
  project ごとの `testIgnore`（レポートから判別できない）。
- Firefox / WebKit、ヘッド付き実行、Playwright の `globalSetup`（setup project は確認済み）、実アプリを相手にした E2E。
- linux-x86_64、macOS ネイティブ。
