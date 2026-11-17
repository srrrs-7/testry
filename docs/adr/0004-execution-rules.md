# ADR-0004: 実行（M2）の判断

- 状態: 採用
- 日付: 2026-11-17
- 対象マイルストーン: M2（Bun / Vitest / command の実行、プロセス制御、スケジューラー、結果統合）

仕様が実装者に委ねている点、および仕様の文面だけでは決まらなかった点の記録。
安全側（広げる・失敗させる・成功と言わない）に倒したものは理由を残す。

## 1. workspace trust は `testry trust` と、リポジトリ外の保存先で実現する

`test` / `verify` は `testry.toml` に書かれたコマンドを実行する。リポジトリを clone しただけで
任意コマンドが動かないよう、**その設定の指紋（config digest）が承認済みのときだけ**実行する。

- `testry trust` は実行されるコマンド（suite ごとの cwd と argv）を表示するだけ。`--approve` で承認、
  `--revoke` で取り消す。承認は「workspace の絶対パス × 設定の指紋」の組で、設定が 1 バイトでも変われば無効。
- 承認の保存先は利用者の状態ディレクトリ（`TESTRY_STATE_HOME` → `XDG_STATE_HOME/testry` →
  `$HOME/.local/state/testry`）。**workspace の中には保存できない**（リポジトリが自分自身を承認できてしまうため。
  workspace 内を指す場合はエラー）。
- CI では、信頼できるジョブ定義が `TESTRY_TRUSTED_CONFIG_DIGEST=blake3:<hex>` を与える。PR が設定を変えると
  指紋が一致しなくなり実行されない。値が指紋の形式でなければエラー（`true` などを承認と見なさない）。
- 未承認は exit 2（`trust_required`）。run ディレクトリも作らない。
- 開発用コンテナは使い捨て（`run --rm`）なので、状態ディレクトリを compose の名前付きボリューム
  `testry-state` に置く。`make clean` はこのボリュームも消す（＝再承認が必要になる）。

## 2. コンテナ外では既定で実行しない

ADR-0002 の未決事項 1 を「既定で拒否」として実装した。`host` provider は環境を準備する前に
コンテナのマーカー（`/.dockerenv`、`/run/.containerenv`）を確認し、無ければ `unsupported_capability`（exit 2）で止まる。
`TESTRY_ALLOW_EXECUTION_OUTSIDE_CONTAINER=1` を明示したときだけ実行する（その場合も未検証である旨を doctor が警告する）。
`plan` / `explain` / `doctor` / `trust` / `runs show` は何も実行しないので対象外。

## 3. ランナーへ渡す環境変数は許可リスト方式

子プロセスが継承するのは `PATH` `HOME` `TMPDIR` `LANG` `LC_ALL` `LC_CTYPE` `TZ` `TERM` `CI` だけ
（`CI` は M3 で追加。理由は ADR-0005 の 7）。それ以外（クラウドの認証情報、トークン、`NODE_OPTIONS` など）は渡さない。
testry が足す変数は固定値のみ（全コマンドに `DO_NOT_TRACK=1` と内部用の `TESTRY_PROCESS_COOKIE`、
Bun に `CI=true`、Storybook に `STORYBOOK_DISABLE_TELEMETRY=1`、Playwright にレポートの出力先）。**利用者が任意の環境変数を suite に渡す手段はまだ無い**（既知の制約。必要になった時点で
設定契約に明示的な許可リストを足す。値はログ・JSON に出さない）。

## 4. アダプターが所有する引数を suite の command に書かせない

レポート・並列数・絞り込み・focus を決める引数はアダプターが付ける。suite の `command` に同じ意味の
フラグ（例: Bun の `--reporter` `--test-name-pattern` `--only` `--bail` `--retry` `--pass-with-no-tests`、
Vitest の `--reporter` `--outputFile` `--maxWorkers` `--testNamePattern` `--changed` `--shard` `--project`
`--passWithNoTests` など。一覧は各アダプターの `RESERVED`）があれば設定エラーにする。
利用者が気づかないうちに実行範囲が狭まる経路を塞ぐため。この検査は最初の suite を起動する前に、
全 suite についてまとめて行う（M3 で前倒しした。ADR-0005 の 2）。

**限界**: ランナーの設定ファイル（`bunfig.toml`、`vitest.config.*`）による絞り込みは検査していない
（設定を実行しない方針のため）。ここで範囲を狭めると testry は検出できない場合がある。
ただし「実行予定のファイルがレポートに無い」ことは下記 6 で検出する。

## 5. ファイル選択は検証済みの版でだけ使う

位置引数の意味はランナーの版で変わりうる。実機で確認した版（`compatibility.toml`: Bun 1.4.4、Vitest 5.1.0）以外では
ファイル選択を使わず **suite 全体を実行する**（`selection_widened` を記録）。版は run のたびに、最初の suite を
始める前に実際のコマンドで確認する（`bun --version` など）。確認できなかった場合も全体実行。

実機で確認した契約（`crates/testry-adapters/tests/runner_contract.rs`）:

| ランナー | ファイル指定 | 帰結 |
|---|---|---|
| Bun 1.4.4 | `./path` は「そのファイルだけ」、`./` 無しは部分一致フィルター | `./<cwd 相対パス>` で渡す。指定外のファイルが実行されたら記録する |
| Vitest 5.1.0 | 位置引数はどの書き方でも部分一致フィルター | 指定したファイルを**含む上位集合**が走る。余分な実行は想定内として扱う |

## 6. 「実行予定のファイル」とレポートの照合

- 実行予定の集合は、ファイル選択なら選択されたファイル、全体実行なら inventory（`include` に一致するファイル）。
- レポートに現れないファイルがあれば `expected_files_missing`。**ランナーが exit 0 なら infrastructure_error**
  （成功と言える根拠が無い）、exit ≠ 0 なら failed（Bun は import できないファイルをレポートに出さず exit 1 に
  するため、これを基盤異常ではなくテスト失敗として扱う）。
- レポートが無い・空・壊れている・DOCTYPE や実体参照を含む・大きすぎる → infrastructure_error（exit code に関係なく）。
- レポート上の失敗と exit code が食い違う場合は**失敗側**に倒し、食い違いを anomaly として残す。
- 1 件もテストが実行されなかった suite は passed にしない。
- command アダプターは exit code だけを扱い、テスト件数とファイルの帰属は `unknown`（0 ではない）。

## 7. focus（`.only`）で他のテストが黙って消えないようにする

- Bun は `CI=true` のとき `test.only` をエラーにする（無いと兄弟テストを黙って落とす）。アダプターは常に `CI=true` を渡す。
- Vitest には `--allowOnly=false` を渡す。

## 8. skip / flaky のポリシー

- 既定では skip / todo は**失敗**（`[policy] fail_on_unexpected_skips = true`）。suite の `known_skips` に
  「ファイル＋タイトルの経路＋状態（skipped / todo）」で完全一致したものだけ許す。ワイルドカードは無い。
- retry 後に通ったテストは `flaky` として保持し、既定では suite を失敗にする（`[policy] fail_on_flaky = true`）。
  どちらも設定で緩められるが、その場合も件数は passed に合算しない。
- Bun の per-test `{ retry: n }` は JUnit XML に現れないため flaky として検出できない（既知の制約）。

## 9. プロセス制御

- 子プロセスは新しいプロセスグループで起動する。timeout・キャンセル時はグループへ SIGTERM、
  猶予（`termination_grace_seconds`）後に SIGKILL。正常終了後に残ったグループ内プロセスも止め、anomaly に記録する。
- 停止を確認できなかったグループは cleanup の残存資源として記録し、その run は passed にならない。
- stdout / stderr は並行して読み捨てずに排出し、保存はストリームごとの上限まで（超過は `output_truncated`）。
- `host` provider は隔離を提供しない。プロセスグループを抜けたプロセス（`setsid`、ブラウザ、開発サーバー）は、
  M3 で追加した環境変数の目印で探して止める（ADR-0005 の 6）。環境を消して起動されたプロセスは追跡できない。

## 10. スケジューラー

- 予約（cpu_slots / memory_mib / 排他キー）が予算内に収まる suite を、plan の順（決定的）で first-fit に投入する。
- 予算より大きい suite は開始前にエラー（待ち続けない）。
- `fail_fast` またはキャンセル後は新しい suite を投入しない。投入されなかった suite は結果に含めず
  （テキスト出力では `not started` と表示する）、その run は `full_suite` を主張しない。
- 予約はスケジューリング上の予算であり、OS レベルの制限ではない。

## 11. 入力の再確認と排他

- plan 作成後・最初の suite を始める直前と、全 suite の終了後にスナップショットを取り直す。
  変わっていれば run は `invalidated`（exit 3）。suite ごとの結果は残すが、assurance は advisory に落とす。
- 1 つの workspace で同時に動ける run は 1 つ。`.testry/locks/workspace.lock` のアドバイザリロックで排他し、取れなければ
  `workspace_busy`（exit 3）。プロセスが落ちればロックは OS が解放する。

## 12. run の保存形式

```text
.testry/runs/<run-id>/
├── run.json          開始時に書く
├── plan.json         実行した plan
├── events.jsonl      1 行 1 イベント。seq は単一 writer が採番
├── result.json       終端レコード。最後に書く
├── environment.json
├── junit.xml         統合レポート
└── tasks/<suite>/{stdout.log, stderr.log, report.*, version-*.log}
```

- ディレクトリは 0700、ファイルは 0600。JSON は一時ファイルへ書いて rename する。
- `result.json` が無い run は**中断**として読む（`runs show` は `state: "interrupted"` を返し、outcome を出さない）。
- run ごとに別ディレクトリなので、前回のレポートを今回の結果として読むことはない（P15）。
- **SQLite による履歴は M5 に送った。** M2 では上記のファイルが唯一の保存先で、`runs show` はそれを読む。
  仕様 13 章の「SQLite とイベントの書き込みが中途で止まる fixture」のうち、ファイル側（途中まで書かれた
  一時ファイルと events）は検証済み、SQLite 側は未着手。

## 13. `plan` は CI のゲート規則の対象外

CI で `affected` / `--suite` を拒否するのは `test` / `verify`（実行して合否を返すコマンド）だけ。
`plan` は何も検証しないので、CI 上でも affected の計画を出力できる。
