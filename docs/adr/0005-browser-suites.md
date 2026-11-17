# ADR-0005: Storybook / Playwright の実行（M3）の判断

- 状態: 採用
- 日付: 2026-11-17
- 対象マイルストーン: M3（Storybook-Vitest アダプター、Playwright アダプター、project の識別、互換性 fixture）

実機（Storybook 10.6.7 + Vitest 5.1.0 のブラウザモード、Playwright 1.65.1、Chromium headless shell、Node 26.13.1）で
確認した事実と、それに基づく判断。確認は `crates/testry-adapters/tests/runner_contract.rs` と
`crates/testry-cli/tests/cli_browser.rs` に残してある。

## 1. Storybook は「Vitest の 1 project 全体」として実行する

- suite は `runner = "storybook_vitest"` と **`project`（必須）** を持つ。実行するのは
  `vitest run --project <project>` で、story をテストに変換する処理とブラウザの操作は Storybook の Vitest addon と
  Vitest に任せる。testry は story ファイルを個別に選ばない（`selection = "file_graph"` は設定エラー）。
- `project` を必須にしたのは、無いと同じ Vitest 設定の他の project（unit など）まで走るため。
- レポートは Vitest の JSON。story 1 つが story ファイルのテスト 1 件になる。`play` 関数の失敗はテストの失敗。
- 実行時に確認できる版は Vitest の版だけ。Storybook 本体の版は確認しない（`compatibility.toml` に、契約テストを
  通した組み合わせを記録している）。

確認できた制約:

- **`tags: ["!test"]` で除外された story はレポートに一切現れない**（skipped にもならない）。testry からは
  「story が減った」ことを検出できない。story ファイル自体が実行されていれば「実行予定のファイルが無い」検査にも掛からない。
- **story ファイルが読み込めない（構文エラーなど）と、Vitest はレポートを書かずに exit 1 で終わる**。
  このとき suite は `failed` ではなく `infrastructure_error`（`report_missing`、exit 3）になる。
  原因は `tasks/<suite>/stderr.log` に残る。合格にはならないが、分類は「テスト失敗」ではない。

## 2. Playwright は常に suite 全体を実行する

- 位置引数は正規表現であり、パスの配列ではない。testry はテストファイルを渡さない。
  ファイル選択が計画に入っていても全体実行へ広げる。
- アダプターが足すのは `--reporter=json`（出力先は環境変数 `PLAYWRIGHT_JSON_OUTPUT_FILE`）、`--output=<run の成果物>`、
  `--workers=<workers>`、`--forbid-only`、および suite に `project` があるときの `--project=<project>` だけ。
- **project の依存関係・setup project・`globalSetup` / `webServer`・retries・ブラウザ設定には触れない**（R18）。
  `--project` を付けても Playwright は依存先の project を実行する（契約テストで確認）。`--no-deps` は設定エラー。
- setup project が失敗すると、依存する project のテストは「attempt の無い skipped」として報告される。
  これを passed に数えない（「実行されなかった」と明記した skipped として残り、suite は failed）。
- 失敗・テスト 0 件・読み込めないファイル・`--forbid-only` 下の `.only` はすべて exit 1 で、レポートの
  トップレベル `errors` に載る。それぞれ失敗 1 件として扱う。
- 1 つの spec が複数の project（ブラウザ）で走ると、project ごとに別のケースになる。project 名はケースの
  識別子の一部として保持し、統合 JUnit では `classname="<suite>[<project>]:<file>"` になる。
- `known_skips` の `project` は、ケースが報告した project（Playwright）または suite の project（Vitest / Storybook）と照合する。

### suite の command に書けないフラグ

`--reporter` `--output` `-j/--workers` `--project` `--forbid-only` `--list`（アダプターが所有）、
`-g/--grep` `-G/--grep-invert` `--shard` `--last-failed` `--only-changed` `--test-list` `--max-failures` `-x`（範囲を狭める）、
`--no-deps`、`--pass-with-no-tests` `-u/--update-snapshots` `--ignore-snapshots` `--repeat-each`、`--ui` `--debug` など。
`-j4` のように値を続けて書いた短縮形も同じフラグとして拒否する。
設定ファイルの指定（`--config=`）、`--retries`、`--timeout`、`--trace`、`--headed` は利用者のものとして通す。

**この検査は最初の suite を起動する前に全 suite について行う。** 以前は各 suite の起動時だったため、
ある suite の設定エラーが、他の suite が走った後に見つかる状態だった（M3 の E2E で発見して修正した）。

## 3. ランナー自身の設定による絞り込みを結果に明示する

Playwright のレポートには解決後の設定が入っている。次のいずれかがあれば、suite の結果に
`scope_limited_by_runner_config` を記録し、その run は **`full_suite` を名乗らない**（結果自体は変えない）。

- `shard`（suite の一部だけを実行）
- `grepInvert`（タイトルで除外）
- `maxFailures`（途中で打ち切り）
- `forbidOnly` が有効になっていない（`.only` が他のテストを隠した可能性）

加えて、絞り込みでテストファイルが丸ごと実行されなかった場合は、登録済みのファイルがレポートに無いので
`expected_files_missing` になり、exit 0 でも合格にならない（ADR-0004 の 6）。

**検出できないもの**: 設定ファイルの `grep`（レポート上は既定値と区別できない）、project ごとの `grep` / `testIgnore`、
Vitest / Storybook 側の設定による除外。これらは「既存設定どおりの範囲」として実行される。

## 4. 「期待どおりの失敗」は合格に数えない

`test.fail()` を付けて実際に失敗したテスト（Playwright の `expected` + `expectedStatus: "failed"`）は、
新しい状態 `expected_failure` として保持する。件数では skip と同じ欄（`skipped`）に入り、passed には入らない。
既定のポリシーでは未承認の expected failure は失敗で、`known_skips` に
`expected_status = "expected_failure"` として**そのテスト・その project** を完全一致で書いたときだけ許す。
skip としての承認は expected failure には効かない。

`test.fixme()` は `todo`、`test.skip()` は `skipped` として区別する。

## 5. 成果物は作業ツリーに書かせない

Playwright は既定で `test-results/` をプロジェクト直下に書く。これは作業ツリーの新しい未追跡ファイルになり、
実行後の入力再確認（P13）で「入力が変わった」と判定されうる。アダプターは `--output` で
`.testry/runs/<run>/tasks/<suite>/output/` へ向ける（権限 0700 の run ディレクトリ内）。

Vitest / Storybook が作るキャッシュ（Vite の `cacheDir`。既定は `node_modules/.vite`）は testry からは向け先を
変えていない。作業ツリー内に作られ、Git で無視されていなければ run は `invalidated` になる（安全側。README に注意書き）。

## 6. プロセスグループの外に出たプロセスも止める

Playwright はブラウザを**別のセッション・別のプロセスグループ**で起動する（実機で確認）。`webServer` の
開発サーバーも同様。ランナーのグループへシグナルを送るだけでは届かない。

- すべてのコマンドに、他と重ならない目印の環境変数 `TESTRY_PROCESS_COOKIE` を渡す。子孫は環境を継承する。
- グループを止めた後、`/proc/<pid>/environ` に同じ目印を持つプロセスを探し、残っていれば SIGTERM → 猶予 → SIGKILL で止める。
  正常終了後に見つかった場合は `survivors_stopped`、止められなかった場合は残存資源として記録する。
- 直後に自分で終了するプロセス（パイプが閉じたことに気づいたブラウザ）を「取り残し」と数えないよう、0.5 秒だけ待ってから判定する。
- 目印はコマンドごとに別なので、同時に走っている他の suite のプロセスには触れない。

実機での観察: Playwright 1.65.1 は SIGTERM でブラウザと `webServer` を自分で片付け、SIGKILL された場合でも
Chromium はパイプ切断で終了した。つまり今回の構成では目印による回収が発動しなくても残らなかった。
回収そのものは、別セッションへ移って SIGTERM を無視するプロセスを使ったテストで確認している
（`crates/testry-adapters/tests/process.rs`）。

限界: 環境変数を消して子を起動するプロセス、`/proc` の無い OS（macOS など）では見つけられない。

## 7. ランナーへ渡す環境の変更

- 継承の許可リストに **`CI`** を加えた。Playwright などの設定は `process.env.CI` で retries や
  `reuseExistingServer` を切り替えることが多く、落とすと CI 上の実行がローカル相当の挙動になってしまう。
- すべてのコマンドに **`DO_NOT_TRACK=1`** を渡し、Storybook の suite には **`STORYBOOK_DISABLE_TELEMETRY=1`** も渡す。
  Storybook は既定で利用統計を外部へ送信する。許可リスト方式では利用者が設定した無効化の変数も落ちてしまうので、
  testry 経由の実行では常に無効にする（送信を増やす方向には倒さない）。

## 8. component と E2E の結果を混ぜない

Storybook（MSW などのモックを含みうる component 検証）と Playwright（E2E）は別の suite・別のアダプター・
別の結果として扱う。片方の成功をもう片方の保証として表示しない。

- affected で差分があるとき、`whole_suite` の suite（Storybook / Playwright）は `inputs` と差分が交わらなくても
  全体を実行する（仕様 15 章。自動では省略しない）。API だけの変更でも unit はファイル単位に絞られ、
  Storybook と E2E は全体が走る。狭い確認には `--suite` を使う。
- ブラウザを使う suite 同士の同時実行は、設定の排他キー（例 `exclusive = ["browser-budget"]`）で制御する。
  testry は Playwright を無制限に並べない（Playwright 内部の並列は `workers` で指定する）。

## 9. 既存の full 結果との比較

`testry verify` の件数は、同じ suite をランナーで直接実行したレポートの件数と一致する
（`the_unified_result_agrees_with_the_runner_run_directly`）。実リポジトリの既存 CI 結果との突き合わせは、
対象リポジトリが無いため未実施。
