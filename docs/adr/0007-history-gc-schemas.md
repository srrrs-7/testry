# ADR-0007: 実行履歴・gc・JSON Schema・ベンチマーク（M5）の判断

- 状態: 採用
- 日付: 2026-11-18
- 対象マイルストーン: M5（SQLite 履歴、成果物の保持と gc、JSON Schema、ベンチマーク）

ADR-0006 により Dev Container provider は実装しない。したがって M5 の「所有資源」は
**run ディレクトリ・plan ファイル・履歴の行・testry が起動したプロセス**だけである。

## 1. 履歴（SQLite）は索引で、正はファイル

- `<storage>/state.sqlite3` に仕様 13 章の表（`schema_migrations` `workspaces` `plans` `runs` `tasks`
  `environment_leases` `artifacts` `duration_samples`）を持つ。rusqlite 0.43.0、SQLite は同梱ソースからビルドする。
- **run の内容の正は run ディレクトリ**であり、終端レコードは `result.json` である。履歴は run・suite・成果物・
  所要時間を、全ディレクトリを読まずに引けるようにするための索引で、`gc` の作業台帳でもある。
- 書き込み順は常に「ファイル → 履歴」。開始時は `run.json` / `plan.json` のあとに `running` の行、
  終了時は `result.json`（最後に書くファイル）のあとに `finished` への更新。どちらも 1 トランザクション。
- 履歴だけを根拠に run を finished と扱うことはない。スキーマの CHECK 制約で「finished ⇔ outcome がある」を強制し、
  一度 finished になった行は後から書き換えない。
- 履歴の読み書きはブロッキング IO なので、実行中は blocking スレッドで行う（起動直後の開封と照合は、
  まだ何も動いていない時点で呼び出しスレッド上で行う）。
- 履歴への書き込みに失敗した場合は保存の失敗（exit 3）として報告する。そのとき `result.json` は既に
  書かれているので、次回の照合で履歴は追いつく。

### 途中で止まった場合の照合（reconcile）

プロセスは 2 つの書き込みの間で止まりうる。`test` / `verify` の開始時と `gc --apply` は、workspace ロックを
取ったうえで（＝他の run が動いていないことを確認したうえで）履歴を run ディレクトリに合わせる。

| 履歴 | run ディレクトリ | 扱い |
|---|---|---|
| `running` | `result.json` がある | 完了していた。ファイルから finished にする |
| `running` | `result.json` が無い | **interrupted**。lease は `abandoned` |
| 行が無い | `run.json` と `plan.json` がある | ファイルから索引に入れる（finished または interrupted） |
| 行がある | ディレクトリが無い | 行を削除し、その旨を報告する |
| `finished` | 何であれ | そのまま（結果を取り消さない・書き換えない） |

途中で切れた `result.json`、別の run の `result.json` は終端レコードとして扱わない（＝interrupted）。
開始記録が無い、または別の run・別の plan のものであるディレクトリは run として扱わない。

`runs list` は読み取り専用なので照合は行わず、finished でない行については表示時にディレクトリと
ロックを見て状態を決める（終端レコードが無いものを finished と表示しない。run が動いていないのに running と表示しない）。

## 2. `runs list` を追加した

仕様 14 章の CLI は `runs show <run-id>` だけだが、run ID を知る手段が `.testry/runs` を覗くことしか無いのは
履歴として不十分なので、`testry runs list [--limit N] [--format json]` を追加した（履歴を新しい順に表示）。
同じ秒に始まった run も実行順に並ぶよう、開始時刻のあとに記録順で並べる（E2E で順序が不定になる不具合を見つけて修正した）。

## 3. gc は「testry のものだと証明できるもの」だけを消す

- **run ディレクトリ**: `<storage>/runs` 直下の実ディレクトリで、名前が run ID であり、その run 自身の
  開始記録（`run.json`）と、そこに書かれた plan（`plan.json`、内容と ID が一致するもの）を持つ場合だけ所有とみなす。
- **plan ファイル**: `<storage>/plans` 直下の通常ファイルで、名前が示す ID と内容が一致する有効な plan の場合だけ。
- それ以外（別名のファイルやディレクトリ、symlink、名前だけ run ID のディレクトリ、別の run の記録をコピーしたもの）は
  `unmanaged` / `unverified` として**報告するだけで触らない**。
- 削除の基準は `storage.history_retention_days`（既定 14 日）。run は終了時刻（中断 run は開始時刻）が
  それより古いとき、plan はそれだけ古く、かつ残す run のどれからも参照されていないときに削除する。
  中断された run も、古くなるまでは証拠として残す。
- `--dry-run` は読むだけで、履歴も含めて何も変更しない（`.testry` が無ければ作りもしない）。
- `--apply` は workspace ロックを取る。取れなければ（run が実行中なら）`workspace_busy`（exit 3）で何もしない。
  削除の直前に所有をもう一度確認し、確認できなくなったものは残して失敗として報告する（exit 3）。
- symlink は辿らない。run ディレクトリの中の symlink はリンク自体を消す。

## 4. 中断された run が残したプロセスを回収する

testry 自身が強制終了されると、起動済みのランナーとその子プロセスは動き続ける。
プロセスの目印（ADR-0005 の 6）に run ID を含め（`TESTRY_PROCESS_COOKIE=<run-id>/...`）、
testry が居なくなった後でも「どの run のプロセスか」を `/proc` から特定できるようにした。

- `gc --dry-run` は、この workspace の run（開始記録で確認できたもの）の目印を持つプロセスを一覧する。
  run が実行中の間は、未完了の run のプロセスを取り残しとは呼ばない。
- `gc --apply` は、ロックを取った状態でそれらを止める（SIGTERM → 猶予 → SIGKILL）。止められなければ失敗として報告する。
- 他の workspace や他の run のプロセスには触れない（run ID が完全一致するものだけ）。

これは仕様の R15（クラッシュ後に中断 run と所有資源を検出する）の、コンテナを持たない構成での実装にあたる。

## 5. JSON Schema は同梱し、実際の出力で検証する

- `schemas/` に draft 2020-12 のスキーマを置く: `plan` `result` `event` `config`（仕様 16 章の 4 つ）と、
  `error` `runs-list` `interrupted-run` `gc`。
- 生成方式は 1 つ: **手で保守する同梱ファイル**。型からの自動生成は採らなかった（値オブジェクトの多くが
  独自の検証つき直列化を持ち、導出したスキーマが実際の制約より緩くなるため）。
- 乖離は `crates/testry-cli/tests/schemas.rs` で防ぐ。実バイナリが実 suite を実行して出した文書
  （plan の各 mode、passed / failed / no_work / timeout の result、全イベント、エラー、履歴、gc）がスキーマに
  合うことと、**壊した文書が拒否されること**（空のファイル選択、passed なのに exit 1、no_work を exit 0、
  outcome を持つ interrupted run、dry-run なのに削除済み、未知のキーなど）の両方を検査する。
- スキーマで表せない制約（予算と suite の関係など）は testry 本体が検査する。スキーマはそれを置き換えない。
- 検証ライブラリ（jsonschema 0.61.2）はテスト専用の依存で、ネットワーク解決と TLS の機能を外している。

## 6. ベンチマーク

- `make bench`（ネットワーク遮断、release ビルド）で `scripts/bench.ts` をコンテナ内の Bun で実行する。
  生データは `docs/benchmarks/raw-<日付>.jsonl`、環境情報は `environment-<日付>.json`、要約は `docs/benchmarks.md`。
- 比較対象は仕様 20 章のうち、コンテナ provider を必要としないもの: ランナーの直接実行、`testry verify`（full）、
  `testry test`（affected）、および規模を変えた `plan`。Host と Colima の比較は行わない（ADR-0006）。
- 各計測はウォームアップ 3 回のあと 30 回。p50 / p95 と最大 RSS、CPU 時間を記録する。
- 直接実行との比較は、ランナーの worker 数を suite の設定に揃えた条件で行う。揃えない比較は
  「オーケストレーションの負荷」ではなく「worker 数の差」を測ってしまう（最初の試走で実際にそうなった）。

## 7. 実装しなかったもの

- 仕様 13 章の `cache/{ast,graph}`（解析結果のディスクキャッシュ）。計測では 5,500 ファイルの affected 計画が
  p50 で 0.1 秒未満であり、キャッシュの失効を正しく扱う複雑さに見合わないと判断した。必要になった時点で
  G15 / G16 にキャッシュ有りのテストを足して導入する。
- `duration_samples` は記録するだけで、スケジューリングには使っていない（v0.2 以降の最適化の材料）。
- ログ・JSON 内の秘匿値のマスク。testry は任意の環境変数を suite に渡せないので、マスクすべき値を知らない。
