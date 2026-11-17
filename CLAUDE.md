# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

# testry — 作業ルール

Rust 製テストオーケストレーター。既存のランナー（Bun test / Vitest / Storybook の Vitest 連携 / Playwright）を
置き換えず、その外側で「何を走らせるか」を計画し、理由を説明し、実行して結果を統合する。
仕様の正本は `CLAUDE_HANDOFF.md`（仕様中の `dle` は `testry` に読み替える）。

## 必ず守ること

1. **ホストでランタイムを実行しない。** `cargo` / `rustc` / `bun` / `node` / テスト内の `git` などは
   すべてコンテナ内で実行する。ホストで使ってよいのは `make`、compose、ファイル操作、読み取り専用のコマンドだけ。
   ファイルの編集にもホストの `python3` / `perl` / `node` などのインタプリタを使わない（編集は編集ツールで行い、
   スクリプトが必要ならコンテナ内で動かす）。
   ホストへツールチェーンをインストールしない（`docs/adr/0002-container-first-execution.md`）。
2. **CLI 名は `testry`。** 設定は `testry.toml`、状態は `.testry/`、クレートは `testry-*`。
3. **実行していない検証を成功と書かない。** `docs/acceptance.md` は実行して通ったものだけ「検証済み」にする。
   未実装の機能が成功を返す経路を作らない（未実装コマンドは exit 2）。
4. **安全側に倒す。** 不確実な依存は省略せず実行範囲を広げる。選択を狭める変更は ADR に理由を書く。
5. 仕様の優先順位: 安全性・不変条件・受け入れ条件 ＞ 既存ルールとの互換 ＞ インターフェイス契約 ＞ 簡潔さ・性能。
   衝突したら勝手に弱めず報告する。
6. **testry は Dev Container を作らず、Docker にも接続しない。** テストは testry と同じコンテナ内のプロセスとして動かす
   （利用者の決定、2026-11-18。`docs/adr/0006-no-dev-container-provider.md`）。コンテナ外での `test` / `verify` は
   既定で拒否する（環境変数で明示したときだけ許可。利用者が追認済み）。この前提を変える提案は、利用者に確認してからにする。

## コマンド（すべてコンテナ内で動く）

```bash
make image      # 開発イメージのビルド
make fetch      # 依存の取得（ネットワークを使うのはここと image / lock だけ）
make ci         # fmt-check + clippy -D warnings + test（ネットワーク遮断・--locked --offline）
make bench      # ベンチマーク（release ビルド・ネットワーク遮断。docs/benchmarks/ に生データ）
make fmt        # 整形
make testry ARGS="plan --mode full"   # 実行系（test / verify）は先に `testry trust --approve` が必要
make shell
```

一部のテストだけを実行する（ゲートと同じくネットワーク遮断。この環境の compose は `docker-compose`）:

```bash
# クレート単位 / 結合テストのファイル単位 / テスト名（部分一致）
docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-adapters --locked --offline
docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-cli --locked --offline --test cli_run
docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-domain --locked --offline --lib -- g18_
```

ネットワークが要る作業（依存の追加など）は `docker-compose -f compose.yml run --rm -T dev <cmd>`。
依存を追加・更新したら `Cargo.lock` を確認し、`make fetch` をやり直す。版を無断で上げない。
完了を報告する前に、必ず `make ci` を最後まで通す。

## 構成

```text
testry-cli ─────────┬──> testry-application ──> testry-domain
                    └──> testry-adapters ─────> testry-application, testry-domain
```

```text
crates/testry-domain       値オブジェクト・グラフ・選択ルール・plan・結果の型（IO 禁止）
crates/testry-application  ポート（trait）とユースケース（計画、実行、スケジューラー、合否の規則）。domain のみに依存
crates/testry-adapters     Git / Oxc / ランナー / レポート解析 / プロセス / 保存の実装
crates/testry-cli          `testry` バイナリ（引数・設定・DI・表示・exit code）
docs/adr                   設計判断（0001〜0007）
docs/architecture.md       `plan` と `test` / `verify` の流れ、結果の決まり方
docs/acceptance.md         受け入れ条件 ↔ テスト ↔ 検証状況（進捗の正）
docs/benchmarks.md         計測方法と結果（生データは docs/benchmarks/）
schemas/                   出力と設定の JSON Schema（tests/schemas.rs が実出力と照合）
scripts/bench.ts           ベンチマーク（コンテナ内の Bun で実行）
compatibility.toml         検証済みのランナー・ランタイムの版と対応機能
tests/fixtures/runners     ランナー契約テスト用の依存（イメージのビルド時に導入）
```

依存方向と IO 禁止は `crates/testry-cli/tests/architecture.rs` が検査する。

### 全体像（複数のファイルにまたがる流れ）

- **`plan`**: `testry.toml` → Git のスナップショット（基準 commit・変更パス・指紋）→ テストの静的列挙 →
  基準版と現在版の依存グラフ（Oxc + oxc_resolver、仮想 FS）→ 純粋関数 `select_affected` → 正規化した
  `ExecutionPlan`（`plan_id` は blake3）。Git とファイル読み取りだけで動き、プロジェクトのコードは実行しない。
- **`test` / `verify`**: 要求の検証 → workspace ロック → plan を内部で作り直す → trust の確認 →
  全 suite の command を事前検査（preflight）→ 環境の準備 → ランナーの版を実コマンドで確認 → 入力の再スナップショット →
  スケジューラー（資源予算・排他キー）が suite を起動 → レポートと exit code の突き合わせ → policy →
  入力をもう一度確認 → cleanup → `result.json`（終端レコード）を最後に書く。
- **保存**: 正は `.testry/runs/<run-id>/`、`state.sqlite3` は索引。書き込みは「ファイル → 履歴」の順で、
  食い違いは次の実行時にロックを取って直す。終端レコードの無い run は interrupted。
- **不確実なときの既定**: 選択は全 suite の全体実行へ広げる。結果は passed にしない
  （レポート欠落は infrastructure_error、対象 0 件は `no_work` = exit 4、入力の変化は `invalidated`）。

## `.claude/` の構成

場所ごとの詳しい規約は rules に分けてある（該当するファイルを触ると読み込まれる）。

| 種類 | 名前 | 用途 |
|---|---|---|
| rule | `rust-code` | Rust の規約、層ごとの制約、テストの命名、依存の扱い |
| rule | `planning` | 差分・依存グラフ・選択（広げる規則、Git の扱い） |
| rule | `runner-adapters` | ランナーごとの実行単位、予約済みフラグ、契約テスト、版の同期 |
| rule | `storage-and-process` | run ディレクトリと履歴、gc の所有確認、プロセス回収、実行環境 |
| rule | `output-contracts` | JSON Schema、stdout の形、exit code、`affected` / `full` の意味 |
| rule | `container-env` | Dockerfile / compose / Makefile を変えるときの前提 |
| rule | `docs` | 「検証済み」の書き方、ADR、計測値の載せ方 |
| skill | `quality-gate` | ゲートや個別のテストをコンテナ内で実行する |
| skill | `mutation-check` | 実装を一時的に壊して、テストが検出することを確かめる |
| skill | `runner-version` | 検証済みのランナー・ランタイムの版を追加・更新する |
| skill | `bench` | ベンチマークを実行し、文書と生データを更新する |
| skill | `milestone-report` | 仕様 23 章の形式で完了を報告する |
| agent | `gate-runner` | ゲートを実行して要約だけを返す（長い出力を主の文脈に入れない） |
| agent | `safety-reviewer` | 「実行していないのに合格」「所有していないものを壊す」経路を探す（読み取り専用） |
| agent | `acceptance-auditor` | `docs/acceptance.md` と実際のテストを照合する（読み取り専用） |

## 進め方

- 仕様 22 章のマイルストーンのうち M0〜M3 と M5 は完了。**M4（Dev Container）は実装しない。**
  以降は仕様 22 章「v0.2 以降」の候補で、着手する項目は計測結果をもとに利用者と決める。
- 安全性に関わる変更（合否、選択、保存、プロセス、trust）は、実装 → skill `mutation-check` →
  agent `safety-reviewer` → `make ci` の順で確かめる。通るだけのテストにしない。
- テストを追加・改名したら `docs/acceptance.md` を同じ変更で直す。出力の形を変えたら `schemas/` も同じ変更で直す。
- まとまった作業の完了時は、仕様 23 章の形式で報告する（skill `milestone-report`）。
- コミットとプッシュは、利用者に頼まれたときだけ行う。
