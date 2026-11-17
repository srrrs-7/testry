# testry — 作業ルール

Rust 製テストオーケストレーター。仕様の正本は `CLAUDE_HANDOFF.md`（仕様中の `dle` は `testry` に読み替える）。

## 必ず守ること

1. **ホストでランタイムを実行しない。** `cargo` / `rustc` / `bun` / `node` / テスト内の `git` などは
   すべてコンテナ内で実行する。ホストで使ってよいのは `make`、compose、ファイル操作だけ。
   ファイルの編集にもホストの `python3` / `perl` / `node` などのインタプリタを使わない（編集は編集ツールで行い、
   スクリプトが必要ならコンテナ内で動かす）。
   ホストへツールチェーンをインストールしない（`docs/adr/0002-container-first-execution.md`）。
2. **CLI 名は `testry`。** 設定は `testry.toml`、状態は `.testry/`、クレートは `testry-*`。
3. **実行していない検証を成功と書かない。** `docs/acceptance.md` は実行して通ったものだけ「検証済み」にする。
   未実装の機能が成功を返す経路を作らない（未実装コマンドは exit 2）。
4. **安全側に倒す。** 不確実な依存は省略せず実行範囲を広げる。選択を狭める変更は ADR に理由を書く。
5. 仕様の優先順位: 安全性・不変条件・受け入れ条件 ＞ 既存ルールとの互換 ＞ インターフェイス契約 ＞ 簡潔さ・性能。
   衝突したら勝手に弱めず報告する。

## コマンド（すべてコンテナ内で動く）

```bash
make image      # 開発イメージのビルド
make fetch      # 依存の取得（ネットワークを使うのはここだけ）
make ci         # fmt-check + clippy -D warnings + test（ネットワーク遮断・--locked --offline）
make bench      # ベンチマーク（release ビルド・ネットワーク遮断。docs/benchmarks/ に生データ）
make fmt        # 整形
make testry ARGS="plan --mode full"   # 実行系（test / verify）は先に `testry trust --approve` が必要
make shell
```

個別に実行する場合:

```bash
docker-compose -f compose.yml run --rm -T dev cargo test -p testry-adapters
```

依存を追加・更新したら `Cargo.lock` を確認し、`make fetch` をやり直す。版を無断で上げない。

## 構成

```text
crates/testry-domain       値オブジェクト・グラフ・選択ルール・plan（IO 禁止）
crates/testry-application  ポート（trait）とユースケース。domain のみに依存
crates/testry-adapters     Git / Oxc / ランナー / プロセス / 保存の実装
crates/testry-cli          `testry` バイナリ（引数・設定・DI・表示・exit code）
docs/adr                   設計判断
docs/acceptance.md         受け入れ条件 ↔ テスト ↔ 検証状況（進捗の正）
docs/benchmarks.md         計測方法と結果（生データは docs/benchmarks/）
schemas/                   出力と設定の JSON Schema（tests/schemas.rs が実出力と照合）
scripts/bench.ts           ベンチマーク（コンテナ内の Bun で実行）
compatibility.toml         検証済みのランナー・ランタイムの版と対応機能
tests/fixtures/runners     ランナー契約テスト用の依存（イメージのビルド時に導入）
```

依存方向と IO 禁止は `crates/testry-cli/tests/architecture.rs` が検査する。

## コード規約

- 本体コードで `unwrap` / `expect` / `panic!` / `todo!` を使わない（lint で deny）。テストは可。
- `unsafe` 禁止。エラーは型で分類し、文字列を解析して分類しない。
- 外部コマンドは argv 配列で起動する。シェル文字列を組み立てない。
- 値オブジェクトのフィールドは private。生成時に不変条件を検証する。
- 受け入れ条件に対応するテストは名前に ID を入れる（例 `g09_rename_reports_old_and_new_path`）。
- 整形は `cargo fmt` の既定。コメントとコード内の文言は英語、ドキュメントは日本語。

## 進め方

- 仕様 22 章のマイルストーン順（M0 → M5）。M0〜M3 完了。**M4（Dev Container）は実装しない**
  （利用者の決定、2026-11-18。`docs/adr/0006-no-dev-container-provider.md`）。M5 も完了。
  以降は仕様 22 章「v0.2 以降」の候補で、着手する項目は計測結果をもとに利用者と決める。
- 出力の形を変えたら `schemas/` も同じ変更で更新する（`tests/schemas.rs` が実出力と照合し、壊した文書の拒否も検査する）。
- testry は Dev Container を作らず、Docker にも接続しない。テストは testry と同じコンテナ内のプロセスとして動かす。
  この前提を変える提案は、利用者に確認してからにする。
- 各マイルストーンの完了時は仕様 23 章の形式で報告する
  （Implemented / Changed files / Tests executed / Tests not executed / Known limitations / Measured performance / Next）。
- コンテナ外での `test` / `verify` は既定で拒否する（環境変数で明示したときだけ許可）。利用者が追認済み。
- ランナーの版を追加・変更するときは、イメージを作り直して `tests/runner_contract.rs` を通し、
  `VERIFIED_VERSIONS`・`compatibility.toml`・Dockerfile を同時に更新する（`tests/compatibility.rs` が照合する）。
- 開発イメージには Chromium（Playwright が入れる headless shell）が入っている。Storybook / Playwright を手で動かすときは
  テレメトリを無効にする（`STORYBOOK_DISABLE_TELEMETRY=1`。testry 経由の実行では自動で無効になる）。
- 通るだけのテストにしない。安全性に関わるテストは、実装を一時的に壊して落ちることを確認する（確認後は必ず戻す）。
