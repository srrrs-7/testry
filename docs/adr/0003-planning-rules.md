# ADR-0003: 設定・スナップショット・解析・選択の実装判断

- 状態: 採用
- 日付: 2026-11-17
- 対象マイルストーン: M0 / M1

仕様が方針だけを定め、実装時に決める必要があった点を記録する。すべて「不確実なら広げる」側に倒している。

## 1. CLI

- `verify` は品質ゲートなので `affected` を常に拒否する（exit 2）。既定は `full`。
- `test` の既定は `policy.local_default_mode`、CI では `policy.ci_default_mode`。
- CI 判定は `--ci` または環境変数 `CI` が `true` / `1`。
- CI では `test` / `verify` の `affected`、`--suite`、base 無しの `shadow` を拒否する。
  `plan` は何も実行せずゲートにもならないため拒否しない（CI は既定 mode の決定にだけ使う）。
- 未実装のコマンド（`test` / `verify` / `runs show` / `gc`）は要求を検証したうえで exit 2
  （`unsupported_capability`）で止まる。成功は返さない。

## 2. 設定（`testry.toml`）

- 既定値を持たない必須キー: `schema_version`、`analysis.global_inputs`、`execution.default_environment`、
  `execution.cpu_slots`、`execution.memory_budget_mib`、各 suite の `selection` と `resources`。
  「何が full へ拡大させるか」「予算」「絞り込みを許すか」は明示的な判断にする。
- 設定ファイル自身は常に global input。`.git/**` と `storage.directory` 配下は常に除外。
- `config_digest` は設定ファイルのバイト列の BLAKE3（コメントの変更でも変わる。保守的）。
- `ci_default_mode = "affected"` はエラー。
- 内部 worker を制御できるランナー（vitest / storybook_vitest / playwright）は `workers` 必須、
  かつ `workers <= resources.cpu_slots`。制御できないランナー（bun / command）に `workers` を書くとエラー。
- `bunx` / `npx` / `pnpx` / `bun x` / `npm exec` / `pnpm dlx` / `yarn dlx` で始まる command はエラー。
- suite 間の依存（`depends_on`）は v0.1 の設定に存在しないので、循環は起こり得ない。

## 3. スナップショット

- 変更集合は「merge-base→HEAD」「HEAD→index」「index→working tree」「未追跡」の和集合。
  打ち消し合う変更も変更として残る。rename は削除＋追加。
- `SourceSnapshotId` は「基準 commit・変更パス集合・現在の各ファイルの BLAKE3」から計算する。
  どの段階（staged / unstaged）で観測したかは含めないので、`git add` だけでは変わらない。
- Git ignore されていても宣言済み input（`global_inputs` / `inputs`）に一致するファイルは指紋に含める。
  ただし基準が存在しないので「変更」にはならない（plan の diagnostics に出す）。
  `.env` を編集しただけでは affected は反応しないため、その場合は `full` を使う。
- 20,000 件を超える ignore 済みファイルが input に一致したら設定エラー（ビルド出力を
  `workspace.exclude` に入れ忘れている可能性が高い）。
- symlink は辿らない（リンク先文字列を指紋化）。submodule は中身を読まず、index の commit だけを指紋化する。
- `WorkspaceId` は正規化した作業ツリー絶対パスのハッシュ。worktree ごとに異なる。

## 4. 依存グラフ

- 解析はスナップショット上の仮想ファイルシステムで行う。実ディスクと `node_modules` は見ない。
  基準版は Git object から読み、チェックアウトも実行もしない。
- 外部パッケージ: リポジトリ内のどこかの `package.json` に依存として宣言されていれば外部扱い
  （内容は lockfile = global input が担保）。宣言の無い bare specifier は未解決 = 診断。
- ワークスペースパッケージ: `package.json` の `name` で判定し、`exports` / `main` を
  リゾルバに解決させる。同名が複数ある場合はリンクせず、import されたら診断にする。
- リゾルバのプロファイルはランナーから決める（bun / vite / node）。プロファイルごとに
  起こり得る export 条件の組をすべて解決し、結果の和集合を取る。和集合は過大近似なので
  選択を広げることはあっても狭めない。
- 拡張子の優先順位に依存しない: `./util` に対して `util.ts` と `util.js` が両方あれば両方に辺を張る。
  `./x.js` に対する `x.ts` も同様。
- モジュールモック（`vi.mock("x")` など）は依存として扱う。
- 解析結果のディスクキャッシュは未実装（プロセス内の parse キャッシュのみ）。
  解決結果は毎回すべて再計算するので、失効漏れは起こらない。

## 5. 選択（affected）

1. 変更なし、またはレビュー済み `non_test_inputs` のみ → `always_run` 以外は実行しない（no_work）。
2. `global_inputs` の変更、グラフ診断が 1 件でもある、どのテストからも到達しない変更がある
   → スコープ内の全 suite を全体実行。
3. それ以外: `whole_suite` と `always_run` は全体。`file_graph` は変更に到達するテストファイルのみ。
4. suite の `inputs` に一致する変更で、(a) その suite のテストから到達しない、または
   (b) パース対象でないファイルである場合、その suite を全体へ拡大する。
   `inputs` は拡大にだけ使い、省略の根拠にはしない。
5. `file_graph` の suite にパース対象外のテストファイルがある場合、その suite は絞り込まない。

### 単調性（P16）について

- 未知依存（診断）を追加しても選択は縮まない: 常に成立（property test あり）。
- 辺を追加しても選択は縮まない: 「到達するテストが無い」ことによる全体実行（上記 2 の 3 つ目、4 の a）を
  除いて成立する。到達テストが無いという *知識の欠如* が理由の拡大は、辺が増えて説明がつけば
  絞り込みに変わるのが正しい挙動である。この範囲を property test と受け入れ対応表に明記している。

### 仕様どおりだが実用上の影響が大きい点

「診断が 1 件でもあれば全 suite へ拡大」は仕様 8 章・`unknown_dependency = "all_enabled_suites"` の
とおりだが、`virtual:` モジュールや `import.meta.glob`、外部パッケージを `extends` する tsconfig が
リポジトリのどこかにあると affected が常に全体実行になる。「その診断のあるファイルに到達する suite
だけを拡大する」方式は健全性を保ったまま絞り込めるが、仕様の変更になるため v0.1 では採用せず、
shadow の実測を見てから判断する。

## 6. plan

- plan は生成時に正規化（全コレクションを整列・重複排除）し、その JSON の BLAKE3 を `plan_id` とする。
- `assurance` は入力ではなく導出値（`full` / `shadow` かつスコープ限定なしのときだけ `full_suite`）。
- 読み込み時は schema_version、正規形、`plan_id` の一致を検査する。一致しない文書は拒否する。
- 保存先は `.testry/plans/blake3-<hex>.json`（ディレクトリ 0700、ファイル 0600、一時ファイル→rename）。
  `.testry` が symlink の場合は書き込まない。
