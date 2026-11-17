---
paths:
  - "crates/**/*.rs"
  - "crates/**/Cargo.toml"
  - "Cargo.toml"
  - "clippy.toml"
---

# Rust コードの規約

## 禁止（lint とテストで検査される）

- 本体コードで `unwrap` / `expect` / `panic!` / `todo!` を使わない（clippy で deny。テストコードは可）。
- `unsafe` 禁止（各クレートが `#![forbid(unsafe_code)]`）。
- エラーは型で分類する。文字列を解析して分類しない。
- 外部コマンドは argv 配列で起動する。シェル文字列を組み立てない。

## 層ごとの制約（`crates/testry-cli/tests/architecture.rs` が検査）

| クレート | 依存してよいもの | 禁止 |
|---|---|---|
| `testry-domain` | なし | ファイル・プロセス・時刻・環境変数などの IO すべて |
| `testry-application` | domain | IO の直接実行。外部との接点は `ports/` の trait に置く |
| `testry-adapters` | application、domain | ユースケースの判断（選択・合否の規則）を持つこと |
| `testry-cli` | すべて | 引数・設定・DI・表示・exit code 以外の規則を持つこと |

- 新しい IO が必要になったら、application に port（trait）を足し、adapters に実装、`testing/` に fake を置く。
- 合否・選択・assurance の規則は domain か application に書く。adapters は事実（レポートの中身、exit code）を返すだけ。

## 型

- 値オブジェクトのフィールドは private。生成時に不変条件を検証し、不正な値を構築不能にする
  （例: 空のファイル集合 `Files([])` は構築も JSON からの読み込みもできない）。
- 「不明」は `Option` や専用の列挙で表す。0 や空文字で代用しない（command アダプターの件数は `unknown`）。

## テスト

- 受け入れ条件（仕様 18 章）に対応するテストは、名前の先頭に ID を入れる
  （例 `g09_rename_reports_old_and_new_path`、`r17_only_one_run_holds_the_workspace`）。
  追加・改名したら `docs/acceptance.md` の該当行も同じ変更で直す。
- 安全性に関わる挙動を足したら、実装を一時的に壊してテストが落ちることを確かめる（skill `mutation-check`）。
- テストの実行は必ずコンテナ内（skill `quality-gate`）。ホストで `cargo` を呼ばない。

## 書式と言語

- 整形は `cargo fmt` の既定（`make fmt`）。
- コメントとコード内の文言（エラーメッセージ、テスト名）は英語。ドキュメントは日本語。

## 依存

- 版を無断で上げない。追加・更新したら `Cargo.lock` の差分を確認し、`make fetch` をやり直す
  （ゲートは `--locked --offline` なので、取得していない依存があると失敗する）。
- 追加した crate は `docs/compatibility.md` の「解析・保存ライブラリ」に用途つきで載せる。
