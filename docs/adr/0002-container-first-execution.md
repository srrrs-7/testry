# ADR-0002: コンテナ優先の実行

- 状態: 採用（未決事項は 2026-11-18 に決着。下記）
- 日付: 2026-11-17
- 対象マイルストーン: M0 以降すべて

## 背景

着手時の指示:「コンテナでの実行を基本として、ホスト側で runtime 実行等は行わない」。

着手時点のホスト（macOS / Colima）には `cargo` / `rustc` / `bun` / `node` / `devcontainer` が無く、
あるのはコンテナエンジン（Docker CLI + Colima）、`docker-compose`、`git`、`make` だけだった。
一方で仕様は実行環境を「Host と Colima 上の Docker + 公式 Dev Container CLI」としている。

## 決定

### 1. 開発・検証はすべてコンテナ内で行う

- ツールチェーンは `Dockerfile` の `dev` ステージに固定する（`rust:1.100.0-slim-trixie`、`rust-toolchain.toml` と一致）。
  ツールチェーンのディレクトリは `dev` ユーザーから書き込めないので、版がずれると黙って別の版を
  取得せずに失敗する。
- ホストで実行するのは `make` と compose だけ。`cargo` / `git`（テスト内）/ `bun` / `node` / `testry` 自身は
  すべてコンテナ内で動く。
- 品質ゲート（`make ci`）は `network_mode: none` + `--locked --offline` で実行する。依存の取得は
  `make fetch` に分離し、ゲートが黙って何かをダウンロードすることはない。

### 2. testry 自体もコンテナ内で動かす

- testry は Linux バイナリとしてビルドし、コンテナ内で実行する（`make testry ARGS="..."`）。
- 仕様の `provider = "host"` は「testry プロセス自身の環境で子プロセスとして実行する」provider である。
  本方針では testry がコンテナ内にいるので、実体は「コンテナ内プロセス実行」になる。
  設定契約との互換のため名称 `host` は変えない。
- `testry doctor` はコンテナ内で動いているかを報告する（外なら warning。成功扱いにはしない）。
- M2 以降、`test` / `verify` はコンテナ外では既定で実行しない（下記の結論 1、ADR-0004）。
- Bun 1.4.4 と Node 26.13.1 も開発イメージに固定して入れてある（ランナー契約テスト用。版は `compatibility.toml`）。
  契約テストの fixture が使う npm パッケージ（Vitest 5.1.0）はイメージのビルド時に lockfile から入れるので、
  ネットワークを遮断したゲートの中では何も取得しない。

### 3. 検証範囲の明記

- 検証済み: Linux/aarch64 コンテナ（Colima 上）での実行。
- 未検証: macOS ネイティブでの testry 実行、Linux/x86_64。コードは POSIX 前提で書いているが、
  動かしていないものを対応済みとは書かない。

## 未決事項だった点の結論（2026-11-18、利用者の決定）

1. **コンテナ外での実行を拒否するか — 既定で拒否する。**
   `host` provider はランナーを起動する前にコンテナ内であることを確認し、外なら既定で拒否する
   （`unsupported_capability`、exit 2）。`TESTRY_ALLOW_EXECUTION_OUTSIDE_CONTAINER=1` を明示したときだけ実行する。
   M2 で安全側の既定として実装し、利用者がこの扱いを追認した。詳細は ADR-0004 の 2。
2. **Dev Container CLI をどこで動かすか — Dev Container を使わない。**
   利用者の決定により、Dev Container provider（仕様の M4）は実装しない。現在の「testry もテストも
   同じコンテナの中で動く」構成を維持する。詳細と影響は [ADR-0006](0006-no-dev-container-provider.md)。

## 影響

- ホストの Docker CLI プラグイン（`docker compose`）が壊れている環境でも動くよう、`Makefile` は
  `docker compose` と `docker-compose` を自動判別する。
- BuildKit（buildx）が無い環境では classic builder でイメージをビルドする。
