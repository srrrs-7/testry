---
paths:
  - "crates/testry-domain/src/**"
  - "crates/testry-application/src/commands/create_plan.rs"
  - "crates/testry-application/src/commands/plan_workspace.rs"
  - "crates/testry-application/src/queries/**"
  - "crates/testry-application/tests/planning.rs"
  - "crates/testry-adapters/src/analysis/**"
  - "crates/testry-adapters/src/source/**"
  - "crates/testry-adapters/src/catalog/**"
  - "crates/testry-adapters/tests/git_snapshot.rs"
  - "crates/testry-cli/tests/cli_plan.rs"
---

# 計画（差分・依存グラフ・選択）

判断の根拠は `docs/adr/0003-planning-rules.md`。

## 不確実なら広げる

次のどれかがあれば、全 suite の全体実行に拡大する。**選択を狭める変更は ADR に理由を書いてからにする。**

- 非 literal の `import()` / `require()`、未解決の import、パース失敗、`import.meta.glob`
- `global_inputs`（lockfile、設定ファイルなど）の変更
- どのテストからも到達しない変更（README などを「無関係」と決めつけない）
- symlink・submodule を通る解決（辿らず、診断にする）

単調性（P16）を保つ: 不明な依存や辺を足して、選択が小さくなってはならない。property テストがある。

## plan

- `plan` は Git とファイル読み取りだけで動く。ランナー設定やテストコードを実行しない、パッケージを入れない、
  環境を起動しない（`cli_plan.rs` が偽ランタイムを PATH に置いて確認している）。
- `full` は差分もグラフも基準 commit も使わない。`affected` / `shadow` は基準 commit と、基準版・現在版の
  2 つのグラフが必須。取得できなければエラーにする（空 diff や `full` として続行しない）。`fetch` はしない。
- plan は正規化してから `plan_id`（blake3）を計算する。列挙順に依存させない（G18）。
- 対象 0 件は `no_work`。成功として扱わない。

## Git の扱い

- 作業ツリー・index・ref を変更しない。外部 diff / textconv を起動しない。
- パスは `-z` 区切りで正確に読む（空白・日本語・改行・引用符）。UTF-8 でないパスは lossy 変換せずエラーにする。
- コンフリクト中の index はエラー。

## 解析を変えるとき

- Oxc / oxc_resolver の版を上げたら `crates/testry-adapters/src/analysis/` のテストを再実行し、
  `ANALYSIS_VERSION` を更新する。
- ランナー間で解決の優先順が違う箇所は、全候補を依存に入れている。片方に寄せない。
- 解析結果のディスクキャッシュは無い（計測上不要。`docs/benchmarks.md`）。導入するなら G15 / G16 に
  キャッシュ有りのテストを足す（`docs/acceptance.md` の注1）。
