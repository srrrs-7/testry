---
name: runner-version
description: testry が「検証済み」とするランナー・ランタイムの版（Bun、Node、Vitest、Storybook、Playwright、Rust）を追加・更新する。「Vitest を上げて」「Playwright の新しい版に対応して」「検証済みの版を増やして」と言われたとき、または Dockerfile や tests/fixtures/runners/package.json の版を変えるときに使う。
---

# 検証済みの版を追加・更新する

「検証済み」は、**その版の実物を起動して契約テストが通った**という意味。表だけを書き換えない。
版を上げるかどうかは利用者が決める（無断で上げない）。

## 版が書かれている場所（すべて一致させる）

| 場所 | 内容 |
|---|---|
| `Dockerfile` | Rust のベースイメージ、Bun、Node の版 |
| `rust-toolchain.toml` | Rust の版 |
| `tests/fixtures/runners/package.json` + `bun.lock` | Vitest、Storybook 関連、Playwright、Vite の版 |
| 各アダプターの `VERIFIED_VERSIONS`（`crates/testry-adapters/src/runners/*.rs`） | ファイル選択を許す版 |
| `compatibility.toml` | 機械可読な正本（`verified_on` の日付も更新） |
| `docs/compatibility.md` | 人が読む表と、確認日 |

食い違うと `crates/testry-cli/tests/compatibility.rs` が失敗する。

## 手順

1. **対象と範囲を確認する。** どのランナーをどの版にするか。既存の版を残して追加するのか、置き換えるのか。
2. **manifest を更新する。** `Dockerfile` / `tests/fixtures/runners/package.json` を Edit ツールで変える。
   `bun.lock` はコンテナ内で作り直す（ネットワークが要る。ホストで `bun install` をしない）:
   ```bash
   docker-compose -f compose.yml run --rm -T -w /workspace/tests/fixtures/runners dev bun install --lockfile-only
   ```
   lockfile の差分を確認し、意図しないパッケージの変更が無いことを見る。
3. **イメージを作り直す。** `make image`（Playwright の版を変えるとブラウザも入れ直される）。
4. **アダプターと表を更新する。** `VERIFIED_VERSIONS`、`compatibility.toml`、`docs/compatibility.md`。
5. **契約テストを通す。**
   ```bash
   docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-adapters --locked --offline --test runner_contract
   docker-compose -f compose.test.yml run --rm -T test cargo test -p testry-cli --locked --offline --test compatibility
   ```
6. **落ちたら、原因を版の違いとして調べる。** 見るところ:
   - レポートの形（JUnit XML / JSON の項目名、状態の表し方）
   - ファイル指定の意味（Vitest は上位集合、Playwright は正規表現）
   - exit code、`.only` の扱い、retry の表し方、テスト 0 件のときの挙動
   - バージョン表示の書式（`parse_version`）
   
   形が変わっていたら、解析を直し、記録 fixture を `tests/fixtures/runner-reports/<runner>-<version>/` に
   **実物の出力から**取り直す。期待値のほうを合わせて通さない。
7. **全体を通す。** `make ci`。
8. **記録する。** `docs/compatibility.md` の確認日と対応機能、必要なら ADR（挙動の判断を変えた場合）。

## やってはいけないこと

- 契約テストを通さずに `VERIFIED_VERSIONS` へ版を足す（未検証の版でファイル選択が有効になり、取りこぼしの原因になる）。
- 検証できなかった版を「検証済み」と書く。通らなければ、未検証のままにして理由を報告する
  （未検証の版でも testry は suite 全体を実行するので、使えなくなるわけではない）。
- 利用者のプロジェクトの lockfile を書き換える（testry は対象リポジトリの依存に触れない）。
- 確認していないブラウザ（Firefox / WebKit）や Storybook フレームワークを「対応」と書く。
