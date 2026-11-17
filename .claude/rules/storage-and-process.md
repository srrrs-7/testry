---
paths:
  - "crates/testry-adapters/src/storage/**"
  - "crates/testry-adapters/src/process/**"
  - "crates/testry-adapters/src/runtime/**"
  - "crates/testry-adapters/tests/process.rs"
  - "crates/testry-cli/src/commands/gc.rs"
  - "crates/testry-cli/src/commands/runs.rs"
  - "crates/testry-cli/src/composition_root.rs"
---

# 保存・プロセス・実行環境

判断の根拠は `docs/adr/0004-execution-rules.md`、`0006-no-dev-container-provider.md`、`0007-history-gc-schemas.md`。
ここは壊れると「実行していないのに合格」「他人のファイルを削除」につながる場所なので、変更のたびに
skill `mutation-check` でテストが本当に検出することを確かめる。

## 保存（`.testry/`）

- **正は run ディレクトリ、SQLite（`state.sqlite3`）は索引。** 書き込みは常に「ファイル → 履歴」の順。
- **`result.json` が終端レコード**で、最後に書く。これが無い run は interrupted であり、
  finished や passed として読んではならない。履歴の側も `finished ⇔ outcome あり` を CHECK 制約で縛っている。
- 途中で止まった場合の修復（reconcile）は、workspace ロックを取ったあとにだけ行う。
  修復は結果を取り消さず、作り出しもしない（ファイルにある事実へ合わせるだけ）。
- 権限はディレクトリ 0700、ファイル 0600。symlink を通して読み書きしない（run ディレクトリ・終端レコード・
  保存先そのものが symlink なら拒否）。artifact の参照は run ディレクトリの外へ出られない。
- 件数が不明な結果を 0 として保存しない（NULL）。
- 履歴のスキーマ版が自分より新しければ、開かず、変更もしない。
- 一覧の並びは `started_at DESC, rowid DESC`（同じ秒に始まった run の順序を決めるため）。
- 承認（workspace trust）はリポジトリの外に保存する。リポジトリが自分自身を承認できてはならない。

## gc

- 削除するのは、testry のものだと**証明できる** run ディレクトリと plan ファイルだけ
  （開始記録が run ID・plan と一致する）。証明できないものは報告するだけで触らない。
- 所有の確認は、削除の直前にもう一度行う。
- 保持期間（`storage.history_retention_days`）を過ぎたものだけが対象。中断された run も同じ期間は残す。
  残す run が参照している plan は消さない。
- `--dry-run` は何も変更しない。`--apply` は workspace ロックを取り、実行中の run があれば拒否する。

## プロセス

- ランナーはプロセスグループで起動する。停止は TERM → 猶予 → KILL。
- グループの外へ出たプロセス（ブラウザ、`webServer` など）は、環境変数の目印
  `TESTRY_PROCESS_COOKIE=<run-id>/…` を `/proc/<pid>/environ` から探して止める。
  目印は run ごと・起動ごとに別の値にし、別のコマンドのプロセスを巻き込まない。
- ランナーへ渡す環境変数は許可リスト（`PATH` `HOME` `TMPDIR` `LANG` `LC_ALL` `LC_CTYPE` `TZ` `TERM` `CI`）と
  `DO_NOT_TRACK=1` だけ。許可リストを広げる変更は利用者に確認する（秘匿値のマスクが未実装のため）。
- 何が起きても cleanup を実行する。cleanup が完了しなかった run は passed にしない。
- stdout / stderr は上限（`max_log_bytes_per_stream`）まで保存し、超えても読み捨てて詰まらせない。

## 実行環境

- provider は `host`（testry と同じコンテナ内の子プロセス）だけ。**testry はコンテナを作らず、Docker に接続しない**
  （利用者の決定。変える提案は利用者に確認してから）。Dev Container の環境を指定した要求は exit 2。
- コンテナの外では既定で `test` / `verify` を拒否する。`TESTRY_ALLOW_EXECUTION_OUTSIDE_CONTAINER=1` のときだけ許可
  （利用者が追認済み。既定を緩めない）。
- 同じ workspace で同時に動ける run は 1 つ（`.testry/locks/workspace.lock`）。取れなければ exit 3 で、何も作らない。
