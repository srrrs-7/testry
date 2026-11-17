# アーキテクチャ

目的は「コード変更から検証結果を得るまでの時間を、安全性を損なわずに短くする」こと。
既存のテストランナーを書き直すものではない。詳細な決定は `docs/adr/` を参照。

## クレート

```text
testry-cli ─────────┬──> testry-application ──> testry-domain
                    └──> testry-adapters ─────> testry-application
                                              └> testry-domain
```

| クレート | 役割 | 主なモジュール |
|---|---|---|
| `testry-domain` | 値オブジェクトと純粋なルール。IO なし | `paths` `ids` `fingerprint` `pattern` `suite` `resources` `selection` `graph` `snapshot` `inventory` `planning` `plan` `results` `environment` |
| `testry-application` | ユースケースとポート | `ports/`（planning・execution・error）、`commands/`（`create_plan` `plan_workspace` `execute_plan`）、`queries/explain`、`scheduler`、`results`、`policy`、`settings`、`catalog`、`testing`（fake） |
| `testry-adapters` | ポートの実装 | `source/`（Git スナップショット・内容参照）、`analysis/`（Oxc 抽出・仮想FS・解決）、`catalog/static_files`、`runners/`（bun・vitest・storybook・playwright・command）、`reports/`（Bun JUnit・Vitest JSON・Playwright JSON・統合 JUnit）、`process/unix`（プロセスグループと、その外へ出たプロセスの回収）、`runtime/host`、`storage/`（plan・run・lock・trust・history・gc） |
| `testry-cli` | `testry` バイナリ | `args` `config` `composition_root` `commands/`（plan・explain・doctor・run・runs・trust・gc）`error` |

## `testry plan` の流れ

```text
testry.toml ──> Settings（検証済み設定）
                   │
Git ──(ls-files / diff -z / ls-tree)──> SourceSnapshot
                   │   基準 commit・変更パス・現在ファイルの指紋・基準ツリー
                   ├──> TestInventory        include パターンによる静的列挙
                   ├──> DependencyGraph ×2   基準版（Git object）と現在版（作業ツリー）
                   │      Oxc で import を抽出 → 仮想FS上で oxc_resolver が解決
                   ▼
            select_affected（純粋関数）──> ExecutionPlan（正規化 → plan_id）
                   │
            .testry/plans/blake3-<hex>.json
```

- `plan` は Git とファイル読み取りだけで動く。ランナー設定の実行、パッケージのインストール、
  環境の起動はしない（E2E テストで偽ランタイムを PATH に置いて確認している）。
- `full` は差分もグラフも使わない。`affected` / `shadow` は基準 commit と両方のグラフが必須で、
  取得できなければエラーにする（空 diff や full として続行しない）。

## 安全側に倒す仕組み

| 状況 | 挙動 |
|---|---|
| 非 literal の `import()` / `require()`、未解決 import、パース失敗、`import.meta.glob` | グラフ診断 → 全 suite を全体実行 |
| lockfile・設定ファイルなど `global_inputs` の変更 | 全 suite を全体実行 |
| どのテストからも到達しない変更（README など） | 無関係と決めつけず全 suite を全体実行 |
| `Files([])` | 型で構築不能。JSON からも読めない |
| 対象 0 件 | `no_work`。成功とは別（`test` / `verify` は exit 4） |
| ランナーの版が未検証 | ファイル選択を使わず suite 全体を実行 |
| plan 作成後の入力変更 | 指紋不一致で `stale_inputs` |
| symlink・submodule を通る解決 | 辿らず診断にする |

## `testry test` / `testry verify` の流れ

```text
要求の検証（CI のゲート規則、--environment）
        ▼
workspace ロック ───────────────────────────── 取れなければ exit 3（何も作らない）
        ▼
plan を内部生成（上の流れ。外部の plan JSON は実行しない）
        │
        ├─ 全 suite にアダプターがあるか ──────── 無ければ exit 2
        ├─ 設定が承認済みか（workspace trust）── 未承認なら exit 2（run ディレクトリを残さない）
        ▼
run.json / plan.json を書く
        │
        ├─ 対象 0 件 ──────────────────────────── no_work（exit 4）。環境もランナーも起動しない
        ▼
環境を準備（host: コンテナ内であることを確認。外なら exit 2）
        ▼
ランナーの版を実コマンドで確認 ──> 未検証の版はファイル選択をやめて suite 全体へ拡大
        ▼
入力を再スナップショット ── 変わっていれば invalidated（1 件も実行しない）
        ▼
スケジューラー（資源予算・排他キー・fail_fast・キャンセル）
   └─ suite ごと: アダプターが argv を作る → プロセスグループで起動 → レポートを読む
                   → exit code とレポートを突き合わせる → skip / flaky のポリシー
        ▼
入力をもう一度スナップショット ── 変わっていれば invalidated
        ▼
環境を解放（cleanup）──> 集計 ──> environment.json / junit.xml / result.json（終端レコードは最後）
```

- 途中で何が起きても cleanup は実行する。cleanup が完了しなかった run は passed にならない。
- イベントは `events.jsonl` に 1 行ずつ追記する（`--format jsonl` では同じ行を stdout にも出す）。
- 判断の詳細は [ADR-0004](adr/0004-execution-rules.md)（実行全般）と [ADR-0005](adr/0005-browser-suites.md)（Storybook / Playwright）。

### ランナーごとの実行単位

| ランナー | 実行単位 | アダプターが足す引数・環境 |
|---|---|---|
| Bun | 検証済みの版ではテストファイル単位、それ以外は全体 | `./<file>…`、`--reporter=junit --reporter-outfile=…`、`CI=true` |
| Vitest | 検証済みの版ではテストファイル単位（上位集合）、それ以外は全体 | `<file>…`、`--project`、`--reporter=json --outputFile=… --maxWorkers=N --allowOnly=false` |
| Storybook | 常に Vitest の 1 project 全体 | Vitest と同じ（`--project` 必須）、`STORYBOOK_DISABLE_TELEMETRY=1` |
| Playwright | 常に suite 全体（setup project と project 依存を維持） | `--reporter=json --output=<run 内> --workers=N --forbid-only`、`--project=`（指定時）、`PLAYWRIGHT_JSON_OUTPUT_FILE` |
| command | コマンド全体。成否のみ | なし |

### 結果の決まり方

| 状況 | suite / run の結果 |
|---|---|
| レポートが無い・壊れている・大きすぎる | infrastructure_error（exit code が 0 でも） |
| 実行予定のファイルがレポートに無い | exit 0 なら infrastructure_error、exit ≠ 0 なら failed |
| レポートは失敗なしだが exit ≠ 0 / レポートは失敗ありだが exit 0 | failed（食い違いを anomaly に記録） |
| テストが 1 件も実行されなかった | passed にしない |
| 想定外の skip / todo / expected failure、retry 後の成功（flaky） | 既定で failed（`[policy]` で変更可。件数は passed に合算しない） |
| setup project の失敗（Playwright） | failed。依存するテストは「実行されなかった」skipped として残り、passed に数えない |
| ランナー自身の設定が範囲を狭めている（Playwright の shard など） | 結果はそのまま、`scope_limited_by_runner_config` を記録し full_suite を名乗らない |
| suite の command がアダプターと矛盾する | 設定エラー（exit 2）。最初の suite を始める前に止まる |
| command アダプターの成功 | passed、件数は `unknown` |
| timeout / シグナル / 起動失敗 | infrastructure_error（exit 3） |
| 実行中に入力が変わった | invalidated（exit 3）。assurance は advisory |
| 対象 0 件 | no_work（exit 4）。環境もランナーも起動しない |
| SIGINT / SIGTERM | cancelled（exit 130）。cleanup 後に終了 |

`full_suite` の assurance を名乗るのは、登録された全 suite を全体実行し、入力が変わらず、cleanup が完了したときだけ
（テストが失敗していても「検証した範囲」は full_suite）。

## 保存されるもの

```text
.testry/                      Git からは常に無視される（0700 / 0600）
├── state.sqlite3             実行履歴（索引）。run・suite・成果物・所要時間
├── locks/workspace.lock      同時に動ける run は 1 つ
├── plans/blake3-<hex>.json   `plan` が保存した計画
└── runs/<run-id>/            1 回の実行の成果物。`result.json` が終端レコード
```

- 正は run ディレクトリで、履歴はその索引。書き込みは常に「ファイル → 履歴」の順。
- プロセスが途中で止まると両者は食い違いうる。次の `test` / `verify` の開始時と `gc --apply` が、
  ロックを取ったうえで履歴をディレクトリに合わせる。終端レコードの無い run は interrupted になり、
  finished として読まれることはない。
- `gc` は保持期間を過ぎた run と plan のうち、testry のものだと証明できるものだけを削除する。
  testry 自身が強制終了されて残ったプロセスは、環境変数の目印（run ID を含む）から特定して停止する。
- 承認（workspace trust）だけはリポジトリの外（利用者の状態ディレクトリ）に保存する。
- 判断の詳細は [ADR-0007](adr/0007-history-gc-schemas.md)。出力の形は [`schemas/`](../schemas/) の JSON Schema。

## 現在の実装範囲

M0（骨格）、M1（差分・グラフ・plan / explain）、M2（Bun / Vitest / command の実行、プロセス制御、
スケジューラー、結果統合、run の保存、workspace trust）、M3（Storybook / Playwright のアダプター）、
M5（実行履歴、`runs list`、gc、JSON Schema、ベンチマーク）まで。
Dev Container provider（M4）は利用者の決定により実装しない（[ADR-0006](adr/0006-no-dev-container-provider.md)）。
該当する環境を指定した要求は exit 2 で止まる。状況は `docs/acceptance.md` を正とする。
