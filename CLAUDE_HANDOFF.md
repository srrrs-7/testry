# DLE：Rust製テストオーケストレーター・実行基盤 実装仕様
## Claudeへの実装引き継ぎ用 / v0.1

作成日：2026-11-17  
仕様の状態：実装前の設計。以下の `dle` コマンド、設定、内部APIは新規に実装する仕様であり、既存製品のAPIではない。  
仮称：Development Loop Engine（DLE）。製品名の確定や商標確認は今回の対象外。

---

## 0. Claudeへの指示と仕様の優先順位

あなたは、この仕様に従ってDLEを実装する。目的は、**コード変更から検証結果を得るまでの時間を、安全性を損なわず短縮すること**である。既存テストランナーをRustで書き直すことではない。

まず既存リポジトリを読み、既存テスト・設定・作業中の変更を保護する。第22章の順序で実装し、マイルストーンごとに実テストを行う。実行していない検証を成功扱いにしない。

優先順位は次の順とする。

1. 本仕様の安全性、不変条件、受け入れ条件。
2. 既存リポジトリで明示されたルールと互換性。ただし上記と衝突したら勝手に弱めず、衝突を報告する。
3. 本仕様のインターフェイス・設定契約。
4. 実装の簡潔さ、保守性、性能。

過去の構想にあった関数単位解析、テスト結果キャッシュ、Warm Pool、Kiri固有最適化、GUI、AI診断は将来機能である。v0.1に混ぜない。特に、**「Safe」という名称による完全性保証は行わず、MVPは `affected / full / shadow` とする。**

この文書にない大規模な機能を先回りして作らない。依存ライブラリのAPIは実装時に公式ドキュメントと採用バージョンで確認する。存在しないCLIフラグ・crate APIを推測で利用しない。

---

## 1. 固定する方針

| 項目 | 決定 |
|---|---|
| 中核 | Rust製の変更解析・計画・実行制御・結果統合 |
| アプリ側 | Bun / Hono / React / Vite / Storybook / MSW / Playwrightを維持 |
| 単体テスト | 既存のBun testとVitestをアダプター経由で利用 |
| コンポーネント | StorybookのVitest連携を利用。Storybook自体を書き換えない |
| MSW | モック基盤。独立したテストランナーとして扱わない |
| E2E | Playwrightを利用。ブラウザエンジンを作らない |
| 実行環境 | 最初はHostとColima上のDocker + 公式Dev Container CLI |
| Dev Containers | 仕様を独自に再実装せず、公式CLIへ委譲する |
| 最初の解析粒度 | テストファイル単位。関数・テストケース単位は後回し |
| 不確実な依存 | 省略せず実行範囲を拡大する |
| PR・リリース | v0.1ではfull検証を必須とする |
| キャッシュ | AST・グラフ・イメージ・依存取得の再利用まで。テスト成功結果の再利用は禁止 |
| 初期GUI / AI / SaaS | 作らない。JSON契約だけ用意する |
| Kiri | 必須依存にしない。Docker API互換性・計測を確認してから別フェーズで対応 |
| データベース | 特定DBに依存しない。Cosmos DBへの対応もプロジェクト側のテスト環境契約で扱う |

Rust化自体による倍率は目標にしない。重複解析、不要な実行、環境再構築、過剰並列を減らした結果を測る。BuildKitには既にキャッシュ最適化の機能があるため、それを利用する。[S10]

### v0.1の成果物

- Rust CLIと4クレートのCargo workspace。
- Git差分・TS/JS依存グラフ・説明可能なテスト計画。
- Bun / Vitest / Storybook-Vitest / Playwright / commandアダプター。
- 資源予算付きスケジューラー、タイムアウト、キャンセル、後始末。
- Host実行と、所有権を明確にした一時Dev Container実行。
- JSON / JSONL / 統合JUnit、実行履歴、性能内訳。
- fixture、異常系、互換性テスト、ベンチマーク手順。
- README、ADR、互換性表、制約一覧。

### v0.1の対象外

独自コンテナランタイム、Lima/Colima/BuildKitの再実装、マイクロVM、Windowsネイティブ対応、任意言語対応、関数単位の完全な意味解析、E2Eの自動絞り込み、分散実行、テスト成功結果キャッシュ、バックグラウンド常駐サービス、MCPサーバー、AI修復、GUI、公開SaaS。

---

## 2. 対応範囲と互換性の扱い

### OS・リポジトリ

- Host：macOSとLinuxのPOSIX環境。Rustの安定版を採用し、実装時に `rust-toolchain.toml` へ正確なバージョンを固定する。
- CPU：arm64 / x86_64を設計対象にするが、未実機検証の組み合わせは「未検証」と記載する。
- ソース：単一Gitリポジトリ内のTS/TSX/JS/JSX/MJS/CJS/MTS/CTS。モノレポ内のパッケージを含む。
- ファイル名：v0.1はUTF-8を要求する。非UTF-8パスを黙って変換しない。
- Windows、サブモジュール内部、Git LFS未取得内容、外部ワークスペースを、対応済みと称さない。

### ランナーバージョン

利用者の既存lockfileを優先する。勝手に最新版へ更新しない。

`compatibility.toml` に、テスト済みのNode・Bun・Vitest・Playwright・Storybook・Dev Container CLIの正確なバージョンと対応機能を記録する。未確認の組み合わせは以下のいずれかにする。

- 絞り込みだけ非対応：suite全体実行へ拡大する。
- レポート解析や終了処理も未確認：`UnsupportedCapability` で停止する。
- 明示的なcommandアダプター：コマンドの成功・失敗だけを扱い、構造化テスト件数や安全な絞り込みを提供したことにしない。

Bunの採用と、Vitest/Playwrightの実行ランタイムをBunに変更することは別の判断である。v0.1の既定では、Bun testはBun、Vitest/Playwrightはプロジェクトで確認済みのNodeで動かす。

---

## 3. アーキテクチャと依存方向

```text
dle-cli ───────────────┬──> dle-application ──> dle-domain
                      └──> dle-adapters ─────> dle-application
                                             └──> dle-domain

dle-domain:
  値オブジェクト / グラフモデル / 選択ルール / 状態遷移

dle-application:
  ユースケース / IOポート / 計画 / スケジューリング / 結果集約

dle-adapters:
  Git / Oxc / ランナー / プロセス / Docker・Dev Container CLI /
  SQLite / ファイル / レポート形式

dle-cli:
  引数 / 設定ロード / DI構築 / 表示 / exit code
```

依存の逆転は「applicationに定義したtraitをadapterが実装する」ことで行う。domainからTokio、CLI、Oxc、Docker、SQLiteへ依存してはいけない。applicationから具体的adapterへ依存してはいけない。

ポートはIO・外部システムとの境界に置く。すべての小関数をtraitにしない。グラフ探索と選択は可能な限り純粋関数とする。アプリケーションサービスへ業務制約を散在させない。

```text
dle/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── crates/
│   ├── dle-domain/src/
│   │   ├── paths.rs
│   │   ├── graph.rs
│   │   ├── selection.rs
│   │   ├── plan.rs
│   │   ├── results.rs
│   │   └── environment.rs
│   ├── dle-application/src/
│   │   ├── ports/
│   │   ├── commands/       # create_plan / execute_plan / cancel / cleanup
│   │   ├── queries/        # explain / read_run / read_metrics
│   │   ├── scheduler.rs
│   │   └── policy.rs
│   ├── dle-adapters/src/
│   │   ├── source/git.rs
│   │   ├── analysis/oxc.rs
│   │   ├── runners/{bun,vitest,storybook,playwright,command}.rs
│   │   ├── runtime/{host,devcontainer_cli}.rs
│   │   ├── process/unix.rs
│   │   ├── storage/sqlite.rs
│   │   └── reports/{json,junit}.rs
│   └── dle-cli/src/
│       ├── main.rs
│       ├── config.rs
│       └── composition_root.rs
├── tests/fixtures/
│   ├── ts-imports/
│   ├── git-changes/
│   ├── runner-reports/
│   └── devcontainer/
├── examples/bun-react-hono/
├── docs/{architecture,compatibility,benchmarks}.md
└── docs/adr/
```

最初から20個のクレートに分割しない。上記4クレートを基本とし、公開境界が明確になってから分離する。

### Rustの実装規約

- 値オブジェクトのフィールドは原則private。生成時に不変条件を検証する。
- Rust慣用の `new` / `try_new` は許可する。グローバルシングルトンや隠れたサービスロケーターは使わない。
- 本体コードで `unwrap` / `expect` / `panic!` を通常のエラー処理に使わない。
- 原則 `#![forbid(unsafe_code)]`。OS機能は安全なAPIを持つ依存crateから利用する。
- エラーは型付き。文字列を解析して分類しない。
- 非同期IOはTokio。CPU負荷の高いパースは制限付きのblocking実行に逃がす。
- ランナー呼び出しはargv配列。コマンド文字列の連結や暗黙のシェル実行は禁止。
- 依存をlockし、循環依存・逆方向依存をCIで確認する。

---

## 4. ドメインモデルと不変条件

| 型 | 内容・不変条件 |
|---|---|
| `WorkspaceId` | 正規化したリポジトリ識別子。Git worktreeも区別する |
| `RepoPath` | リポジトリ相対UTF-8パス。絶対パス、NUL、`..`による脱出を拒否 |
| `SourceSnapshotId` | 関連ファイル内容・追加削除・設定を含む入力指紋 |
| `BaselineId` | 解決済みGit commit ID。可変ブランチ名だけで保存しない |
| `SuiteId` | 設定内で一意。空文字禁止 |
| `TestFileId` | `suite_id + runner_project + repo_path`。異なるプロジェクトを混同しない |
| `PlanId` | 入力指紋・解析ポリシー・計画のcanonical形式から生成 |
| `RunId` | 実行ごとに一意。PlanIdとは別 |
| `TaskId` | run内で一意な実行単位。v0.1はsuite単位のバッチ |
| `EnvironmentLeaseId` | 専有する実行環境の貸出ID。コンテナIDだけにしない |
| `Selection` | `All / Files(非空集合) / Omit(理由)` |
| `SelectionReason` | 直接変更、逆依存経路、設定変更、未知依存、必須検証など |
| `GraphDiagnostic` | 未解決import、動的依存、パース失敗、未対応機構など |
| `ResourceRequest` | CPU予約数、メモリ予約量、排他キー。負数・0 CPUを拒否 |
| `ProcessExit` | 正常終了code、signal終了、timeout、cancelledを区別 |
| `TaskOutcome` | passed / failed / skipped / flaky / timed_out / cancelled / infrastructure_error |
| `RunOutcome` | passed / failed / no_work / invalidated / cancelled / infrastructure_error |

### 禁止する状態

- `Files([])`。空配列をランナーへ渡して意図せず全件実行することを防ぐ。
- 解析エラーを空グラフ・影響なしに変換する。
- 実行対象0件を、検証成功と同一視する。
- 終了code=0だけで、壊れたレポートや起動できていないテストを成功にする。
- skip / todo / flaky / cancelledを通常のpassedに畳み込む。
- テスト成功後にcleanupが失敗したrunを、完全な成功として終了する。
- 現在の環境と異なるfingerprintのplanを実行する。
- 未取得の資源を解放する、同じleaseを二重貸出する。
- このrunが所有していないコンテナ・ボリュームを削除する。

`full` は「設定された有効suiteすべて」の意味であり、リポジトリに存在する未登録テストまで発見・保証する意味ではない。READMEとレポートに対象suiteを明記する。

---

## 5. インターフェイス契約

以下は実装するRustインターフェイスの契約である。型の意味は本章の表で定義する。これはライブラリの完成コードやコンパイル検証済みスケルトンではない。実装では各型を適切なクレートに分離し、契約テストを追加する。

`async fn` traitの動的ディスパッチ方法を曖昧にしないため、ポートはobject-safeなboxed futureで示す。`async-trait`を採用する場合も同じ振る舞いを維持する。

```rust
use std::{future::Future, pin::Pin, sync::Arc};

pub type PortFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, PortError>> + Send + 'a>>;

pub trait Cancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn cancelled<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

pub struct OperationContext {
    pub run_id: RunId,
    pub cancellation: Arc<dyn Cancellation>,
    pub deadline: Option<std::time::Instant>,
}

pub trait SourceRepository: Send + Sync {
    fn snapshot<'a>(
        &'a self,
        request: &'a SnapshotRequest,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, SourceSnapshot>;
}

pub trait DependencyAnalyzer: Send + Sync {
    fn analyze<'a>(
        &'a self,
        snapshot: &'a SourceSnapshot,
        profile: &'a AnalysisProfile,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, DependencyGraph>;
}

pub trait TestCatalog: Send + Sync {
    fn collect<'a>(
        &'a self,
        snapshot: &'a SourceSnapshot,
        suites: &'a [SuiteDefinition],
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, TestInventory>;
}

pub trait RunnerAdapter: Send + Sync {
    fn kind(&self) -> RunnerKind;
    fn capabilities(&self, version: &ToolVersion) -> RunnerCapabilities;

    // Pure command construction. No subprocess/network execution here.
    // It may expand a selection; it must never silently narrow one.
    fn prepare(
        &self,
        suite: &SuiteDefinition,
        selection: &Selection,
        env: &ExecutionDescriptor,
        artifacts: &ArtifactLocations,
    ) -> Result<PreparedInvocation, AdapterError>;

    // Structured report normalization. Never parse ANSI console output as truth.
    fn normalize(
        &self,
        invocation: &PreparedInvocation,
        process: &ProcessResult,
        reports: &ReportBundle,
    ) -> Result<SuiteResult, AdapterError>;
}

pub trait RuntimeProvider: Send + Sync {
    fn prepare<'a>(
        &'a self,
        request: &'a EnvironmentRequest,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, EnvironmentLease>;

    fn execute<'a>(
        &'a self,
        lease: &'a EnvironmentLease,
        command: &'a CommandSpec,
        events: &'a dyn EventSink,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, ProcessResult>;

    fn release<'a>(
        &'a self,
        lease: EnvironmentLease,
        disposition: ReleaseDisposition,
        cleanup: &'a CleanupContext,
    ) -> PortFuture<'a, CleanupReport>;
}

pub trait EventSink: Send + Sync {
    fn append<'a>(&'a self, event: RunEvent) -> PortFuture<'a, ()>;
}

pub trait RunStore: Send + Sync {
    fn create<'a>(&'a self, record: &'a RunRecord) -> PortFuture<'a, ()>;
    fn finish<'a>(&'a self, result: &'a RunResult) -> PortFuture<'a, ()>;
    fn read<'a>(&'a self, run_id: &'a RunId)
        -> PortFuture<'a, Option<RunResult>>;
}

// Pure domain/application functions.
pub fn create_plan(
    snapshot: &SourceSnapshot,
    baseline_graph: &DependencyGraph,
    current_graph: &DependencyGraph,
    inventory: &TestInventory,
    suites: &[SuiteDefinition],
    policy: &SelectionPolicy,
) -> Result<ExecutionPlan, PlanningError>;

pub fn validate_execution_plan(
    plan: &ExecutionPlan,
    inventory: &TestInventory,
    budgets: &ResourceBudget,
) -> Result<(), PlanningError>;
```

### DTOの必須フィールド

| DTO | 必須フィールド |
|---|---|
| `SnapshotRequest` | workspace root、mode、base ref、include dirty、明示的suite scope |
| `SourceSnapshot` | workspace ID、baseline commit、input fingerprint、変更のold/new path、現在/基準版のファイル内容参照、取得診断 |
| `DependencyGraph` | nodes、consumer→dependency edges、解析fingerprint、diagnostics、resolver profile |
| `TestInventory` | suite別ファイル集合、runner project、由来、inventory fingerprint、未確認項目 |
| `SuiteDefinition` | id、runner、cwd、command argv、include、inputs、project、selection policy、known_skips、timeout、resource request |
| `RunnerCapabilities` | full実行、file選択、file attribution、structured report、worker制御、対応バージョン |
| `ExecutionPlan` | schema version、plan ID、snapshot ID、baseline、mode、scope、suite plans、fallback理由、資源、config digest |
| `PreparedInvocation` | task ID、要求selection、実効selection、argv、期待ファイル、report path、許容追加ファイル |
| `CommandSpec` | program、args、workspace-relative cwd、許可環境変数、timeout、stdout/stderr方針 |
| `EnvironmentRequest` | provider、workspace、config fingerprint、lease ownership、readiness、trust承認情報 |
| `ExecutionDescriptor` | provider、OS/arch、実際のtool versions、workspace path mapping、image ID/digest、能力 |
| `EnvironmentLease` | lease ID、owner run ID、provider、専有資源一覧、descriptor、状態 |
| `ProcessResult` | exit種別、開始/終了時刻、duration、stdout/stderr artifact、切捨て有無 |
| `ReportBundle` | 形式、サイズ制限検証済みbytesまたは安全なartifact参照、欠落/破損情報 |
| `SuiteResult` | suite/task ID、結果、case/file帰属、実行/skip/flaky件数、attempts、異常、artifact refs |
| `CleanupContext` | 元のキャンセルとは独立したdeadline、ownership、retry上限 |
| `CleanupReport` | 削除/残存資源、失敗理由、再実行可能なcleanup token |
| `RunRecord` | run ID、plan ID、開始時刻、mode、scope、workspace ID、初期状態、artifact root |
| `RunResult` | plan/run ID、mode、全suite状態、保証範囲、metrics、環境fingerprint、cleanup、最終exit code |

`TestCatalog::collect` はv0.1ではファイル列挙と静的設定だけを使い、ランナー設定やテストコードを実行しない。実行preflightでrunner由来のinventoryを取得する場合は、trust承認後にRuntimeProviderを経由する。静的inventoryとの差は無視せず、選択の拡大または設定不整合として扱う。

`full` では差分・基準版グラフを省略してよいが、現在の入力fingerprintとsuite登録は必須。`affected / shadow` のbaseline取得失敗をfullと同じ扱いで黙って続行しない。

### 型付きエラー

`Configuration` / `TrustRequired` / `SourceUnavailable` / `UnsupportedCapability` /
`Planning` / `Spawn` / `Timeout` / `Cancelled` / `InvalidReport` /
`Storage` / `StaleInputs` / `CleanupFailed` を最低限区別する。

テストのassertion失敗は `PortError` ではなく `TaskOutcome::Failed`。
異常な依存グラフは `GraphDiagnostic` としてfallback判断に渡せるが、ソースを取得できず何を実行するか判断できない場合はエラーで止める。

---

## 6. 差分取得と入力の固定

### Git差分

ローカル既定のbaseはHEADとする。これは未コミット変更を対象とすることをCLIヘルプに明記する。ブランチ全体を対象にする場合は `--base origin/main` 等を明示する。

baseを指定したときは `git merge-base <base> HEAD` の結果を固定して保存する。CIのshadowではbaseを明示必須にする。shallow clone等でbaseが取得できない場合、勝手に空diff扱いにせず、明確なエラーとfull実行の案内を返す。勝手にネットワークfetchしない。

次を合併する。

- merge-base → HEAD のコミット差分。
- HEAD → index のstaged差分。
- index → working tree のunstaged差分。
- 未追跡ファイル。ただし既定のGit ignoreを尊重した列挙だけでは入力完全性を保証しないため、明示的なsuite inputsも別途検査する。

Gitの出力はNUL区切りで処理する。v0.1はrenameを削除＋追加として扱ってよい。`--no-ext-diff` / `--no-textconv` を付け、Gitによる外部コマンド実行を避ける。[S7][S8]

作業ツリーへ `reset` / `clean` / `stash` を実行してはいけない。コンフリクト中のindexはエラーにする。基準版の解析用ファイルはGit objectから読む。基準版のコードや設定を実行しない。

### snapshotの一貫性

plan生成時、実行直前、実行終了時の入力fingerprintを照合する。途中で対象コード・設定・lockfileが変わった場合は `invalidated / StaleInputs` とし、その結果を現在のコードの検証結果として使わない。

v0.1のHostとbind mountは完全な不変スナップショットではない。実行前後の検査も敵対的な変更の完全な検出を保証しない。厳密な再現性が必要なCIでは、クリーンな専用checkoutを使用する。

同一workspaceのDLE実行は排他ロックする。別worktreeは別workspaceとして扱う。エディターの変更までロックできるとは主張しない。

---

## 7. 依存グラフの構築

### エッジの向き

`consumer → dependency` に統一する。

```text
user.test.ts → user.ts
user.ts      → validation.ts
```

`validation.ts` の変更時は逆向きにたどり、`user.test.ts` を選択する。

現在版だけでなく、基準版と現在版のグラフを合併して探索する。削除・リネームされた依存の利用側を失わないためである。

### v0.1で解析するもの

- `import ... from "literal"`、side-effect import。
- `export ... from "literal"`、`export * from "literal"`。
- `import("literal")`、`require("literal")`。
- type-only import/exportも安全側で依存に含める。
- tsconfig paths/baseUrl、対応確認済みworkspace package。
- JSON/CSS等のliteral importは入力ノードとして保持する。ただしローダー内部の意味解析は行わない。

Oxcは構文解析に、resolverはモジュール解決に利用する。ASTを構築しただけで型検査や実行時依存の完全な解析ができるとは扱わない。[S11][S12]

### 必ず不確実性として扱うもの

非literal dynamic import/require、未解決alias、未対応Viteプラグイン、任意の仮想モジュール、未対応のpackage条件解決、`import.meta.glob`の未対応形式、動的なファイル読み込み、環境依存のmodule解決、repo外の参照、生成物の由来不明、パース失敗。

テストがファイル・環境変数・外部APIを読む関係は、importグラフだけでは表せない。`inputs` / `global_inputs` に宣言し、対応不能な経路はfallbackする。

suiteごとにブラウザ/Node/Bun、条件export、tsconfig等のresolution profileを区別する。resolverが実際のランナーの解決規則と一致することをfixtureで確認できない場合、そのsuiteのfile選択を無効にする。

追加・削除によって、変更されていないimportの解決先が変わる場合がある。ファイル集合、package.json、tsconfig、resolver設定変更時は全体の解決関係を再計算する。ASTのcontent cacheだけは再利用してよい。

### キャッシュキー

`content hash + parser version + source type + parser options` をASTキャッシュキーとする。
解決結果はこれに `resolver version + profile + config digest + relevant directory inventory` を加える。

mtimeだけでキャッシュを正当化しない。関係が不明なファイルを「依存なし」として保存しない。

---

## 8. テスト選択ポリシー

### 3つのmode

| mode | 実行 | 用途 |
|---|---|---|
| `affected` | グラフで絞る。ただし未知依存時は拡大する | ローカル・AIの反復。単独ではCI品質ゲートにしない |
| `full` | 有効suiteをすべて実行 | PR、merge、リリース |
| `shadow` | affected計画を作るが、実行はfullを1回行う | 選択精度の観測。CIゲートに使用可能 |

shadowでaffected→fullと同じ可変環境に二重実行してはいけない。まずfullを1回実行し、「候補計画に含まれないテストの失敗」を記録する。これはselection gap候補であり、変更が原因の不具合や完全性を直接証明するものではない。

### 決定順序

1. 設定・trust・base・入力を検証する。
2. `full / shadow` の実行範囲は全有効suite。
3. `affected` でもglobal input変更・未知依存・解析不完全は全有効suiteへ拡大。
4. `always_run=true` のsuiteは省略しない。
5. Bun/Vitestのfile選択は、対応確認済みのresolver・inventory・report機能がある場合のみ使う。
6. 変更されたテストファイル自身、旧/新グラフの逆依存テストを選択する。
7. StorybookとPlaywrightはv0.1ではsuite全体。ケース単位や通信履歴による絞り込みは禁止。
8. コード変更があるのに対象テストが見つからない場合は、影響なしと決めつけず全有効suiteへ拡大する。
9. 変更なし、またはレビュー済みのnon_test_inputsだけなら `no_work` を許可。ただしalways_runは実行する。

`non_test_inputs` は既定空。Markdownを自動的に無関係扱いにしない。READMEをテストが読むケースもある。

`--suite` によるscope限定はローカルだけで許可し、レポートに明示する。CIのfull/shadowではscope限定を拒否する。

### global fallbackの例

lockfile、package.json、TS/Vite/Vitest/Playwright設定、共有setup、MSWの共通handler、Storybook設定、Dev Container/Docker設定、API schema、DB migration・初期データ、認証共通コードの明示指定、環境設定。

global inputとnon_test_inputsが重複した場合はglobalが優先する。

### 意図しないテスト除外を防ぐ

- カバレッジがないことを「依存しない」根拠にしない。
- AIのconfidenceでテストを除外しない。
- 「失敗したことがない」「最近通った」は省略理由にしない。
- 新規/変更テスト、共有fixture、setupの変更を必ず含める。
- runtimeで要求より広く実行するのは許可。狭くするのは禁止。
- `plan` の選択理由と `run` の実際の実行集合を両方保存する。
- runtimeで選択集合が満たされない場合は `InvalidReport`。部分成功を合格にしない。

---

## 9. ランナーのアダプター契約

| adapter | v0.1の選択 | 正本のレポート |
|---|---|---|
| Bun | 対応版でテストファイル単位 | JUnit XML |
| Vitest | 対応版でテストファイル単位 | JSON |
| Storybook-Vitest | 対象Vitest project全体 | Vitest JSON |
| Playwright | 対象設定のproject/dependencyを維持して全体 | JSON |
| command | コマンド全体 | exit状態とartifact。テスト件数はunknown |

BunはファイルフィルターとJUnit出力を提供する。Vitestはlist/related等を持つが、静的依存には制約がある。Playwrightの位置引数は正規表現であり、単なるパス配列と同一視しない。[S1][S2][S3][S4]

### 共通契約

- `command` は固定argv配列。`bunx` / `npx` による暗黙のダウンロードは禁止する。
- report、worker、selectionを変更する引数はadapterが所有する。設定内で矛盾するオプションを指定したらConfiguration error。
- CLIやconfigにtestNamePattern、grep、shard、only-changed等が入っていないか検査する。検査できない場合は選択の保証範囲を制限し、fullを勝手に狭めない。
- `--passWithNoTests` / `--pass-with-no-tests` に相当する設定で0件成功を許可しない。
- 実行ごとに固有report pathを使う。前回のreportを読むことを防ぐ。
- runnerのプロジェクト名・browser/project dimensionを結果から落とさない。
- 選択ファイル内の全テストを実行し、ケース単位では省略しない。
- skip/todo/expected failure等の明示的な除外は、既存suiteの承認済みbaselineとして記録する。新規の予期しないskipはpolicy違反にする。
- retriesでpassしたケースはflakyとして保持する。統合結果で初回失敗を消さない。
- DLEはv0.1で自動再実行を追加しない。既存ランナーのretriesは尊重し、全attemptを保持する。
- snapshotの自動更新は行わない。
- CIではfocus/only指定を禁止する。採用ランナーが提供する禁止機能を優先し、該当バージョンで検証する。対応できない検証を対応済みと表示しない。既存設定でテストが狭まる場合は、その事実を対象範囲として明示する。

### ファイル指定の正確性

Bun/Vitestの位置引数の意味を、そのバージョンの契約テストで検証する。globやsubstringを「完全一致」と思い込まない。実行集合が要求の上位集合になることをレポートで照合する。

`foo.test.ts` と `foo.test.tsx`、空白、角括弧、日本語、似たprefixのファイルを含むfixtureを用意する。正確な対応ができない版は全suiteへ拡大する。

Bunの構造化レポートからファイル帰属が確認できない版では、安全なfile選択を提供したことにしない。reportのバージョン差は保存済み実データfixtureで検証する。[S2]

### Storybook・MSW・Playwright

Storybookは既存のVitest addon設定を利用する。ストーリーをテストにする変換やbrowser modeは公式側に任せる。[S5]

Playwrightのproject dependencies、globalSetup/teardown、webServer、retry、browser設定をDLEが勝手に無効化しない。既に内側で並列実行できるため、DLEが無制限に複数Playwrightを立ち上げない。[S13]

MSWの成功で実APIが動作したことにはしない。component/UI検証と実API E2Eは別suite・別保証として表示する。

---

## 10. スケジューラーとプロセス制御

### 実行単位

v0.1は「suiteごとのランナー起動」をtaskとする。選択ファイルを可能な限り1回のランナー起動にまとめる。テストケースごとにプロセスを作らない。

同一Dev Container lease内のtaskはv0.1では直列にする。理由は、キャンセル時に環境全体を安全に停止できるようにするためである。Hostでは資源予算の範囲でsuite並列を許可する。コンテナ並列は独立leaseを持つ将来機能にする。

### 資源予約

- `sum(cpu_slots) <= cpu budget`
- `sum(memory_mib) <= memory budget`
- 排他キーは同時取得不可。
- 必要量が全予算を超えるtaskは、待ち続けず計画時にエラー。
- 複数資源はスケジューラーが一括予約する。semaphoreを順不同で取得してdeadlockを作らない。
- `workers` とsuiteのCPU予約を整合させる。runnerの内側のworkerを制御できない場合、保守的に大きく予約し、並列化を制限する。
- 予約は論理的予算。Hostの厳密なCPU/メモリ上限とは区別する。

ready taskは安定した順序で投入する。過去の実行時間による賢い最適化はv0.2以降。v0.1で履歴依存の非決定的プランを作らない。

### タイムアウト・キャンセル

Hostでは専用process groupを作り、SIGTERM → grace period → SIGKILL → wait/reapの順で回収する。親プロセス1つのkillだけで子孫も消えたと判断しない。Tokioのkill-on-dropだけに依存しない。[S9]

非協調的プロセスがsessionを分離する等のケースまでHostで安全に隔離できるとは主張しない。v0.1は信頼済みコード向けである。

Dev Containerではホスト側の `devcontainer exec` をkillしただけで完了としない。専有のDLE-managed containerを停止・削除し、残存プロセス/資源を確認する。v0.1は同じlease内で別taskを並行させない。

cleanupはキャンセル済みtokenと別のdeadlineを持つ。キャンセルを受けたからといってcleanupまで中断しない。cleanup失敗は保存し、次回 `gc` で所有権を確認して回収できるようにする。

### 出力

stdout/stderrを同時にdrainし、pipe詰まりを防ぐ。無制限にRAMへ収集しない。ログ保存上限を超えた場合もdrainを続け、保存の切捨てを記録する。

構造化reportの欠損と、人間向けログの切捨ては区別する。reportを切り捨てたまま成功にしない。

---

## 11. Dev Container実行基盤

### v0.1の戦略

独自Dev Container実装を作らず、公式CLIを呼ぶ。公式CLIは仕様の参照実装である。[S6]

v0.1で保証するのはsingle-containerのimage/Dockerfile構成。Features・remoteUser・remoteEnv・ライフサイクルは公式CLIに委譲し、対応fixtureで検証する。

Docker Compose、ホスト上の任意コンテナへのattach、複数サービスの共有環境、リモートDockerはv0.1の管理対象外。検出したら明示的に非対応とし、既存Host実行へ手動で切り替えられるようにする。黙って設定を無視しない。

### Colimaとの関係

Colimaは既に起動済みの環境を使う。勝手にVMを起動・削除・設定変更しない。Docker contextは設定で明示する。`docker context use` で利用者のグローバル設定を変更しない。

contextから得たlocal Unix socketを、この子プロセス群だけの接続先として使う。矛盾する `DOCKER_HOST` / `DOCKER_CONTEXT` を隔離し、接続したEngineの識別情報を保存する。SSH/TCP/TLS接続はv0.1では拒否する。

### 所有権

run/leaseごとに一意なラベルを与える。

```text
dle.managed=true
dle.workspace_id=<workspace-id>
dle.run_id=<run-id>
dle.lease_id=<lease-id>
```

CLIのid-labelで既存の編集用Dev Containerと区別し、返されたcontainer IDを再検査する。既存のコンテナを削除するオプションは使用しない。コマンドと引数は採用したCLI版のhelp/ソースで確認する。[S15]

削除対象は「永続レコードに所有が記録され、実物のラベルも一致する資源」に限定する。container名のprefix一致だけで消してはいけない。共有named volume、ホストbind mount内のデータ、他のworkspaceは削除しない。

### ライフサイクルの完了

`up` が返っただけで、すべての初期化が完了したとは判断しない。Dev Containerには非同期に進行し得るライフサイクルとwaitForがある。[S14]

v0.1では以下を満たす。

1. CLIへの委譲で規定の順序を維持する。
2. 必要なpostCreate/postStartまで待つ仕組みを、採用CLI版で確認して利用する。
3. 待機設定のoverrideが必要なら、利用者ファイルを変更せずrun専用設定で行い、内容を記録する。
4. 全初期化の成功を確認した後、設定されたreadiness commandを実行する。
5. readiness失敗はテスト失敗ではなく環境準備失敗。
6. 速度のためにlifecycle commandを省略しない。

`initializeCommand` はホスト側で動くため既定拒否。明示的承認がある場合だけ利用する。trustの対象と変更したdigestを記録する。[S14]

### 状態遷移

```text
Requested → Preparing → Ready → Running → Cleaning → Released
                  \         \       \         \
                   └─────────┴───────┴─────────→ Failed
```

v0.1はdisposable。各runで新しい環境を使い、イメージ・BuildKitキャッシュ等のみ再利用する。既存の開発環境を使い回す高速化と混同しない。

### v0.2以降のWarm Pool条件

`image digest + OS/arch + devcontainer resolved config + toolchain +
lockfile + setup/reset recipe version + trust domain` を環境キーとする。

単に同じイメージだから再利用可能とはしない。mutable filesystem、DB、背景process、ポート、credentialをresetして検証し、成功した環境だけReadyへ戻す。失敗時は破棄する。browserのcacheやログインsessionも対象。

異なる利用者・repo・trust domain間でwarm環境を共有しない。snapshot/COWはruntimeが本当に提供する場合だけ対応し、Docker APIで一般に使えると仮定しない。

---

## 12. キャッシュ・DB・テストデータ

### v0.1で許可

- 内容指紋によるASTの再利用。
- resolver条件を含むグラフの再利用。
- OCIイメージ・BuildKit・パッケージ取得cacheの利用。
- 実行時間履歴の保存。

### v0.1で禁止

- 「以前passした」ことによるテスト実行省略。
- 外部APIや時刻依存のテスト結果cache。
- flakyをcacheで隠す。
- credentialそのものを共有cache key・ログへ格納する。
- lockfileが同一だから別OS/archの成果物を同一扱いする。

### DB

DLEはテストデータの意味を知らない。DBの準備/resetは、プロジェクトの明示的なfixtureまたは後続バージョンのhookで実装する。

Cosmos DBを使う場合も、SQL DBのtransaction rollbackを前提にしない。runごとのnamespace/識別子、衝突回避、削除範囲、失敗時cleanupをプロジェクト側が定義する。共有DBしか使えないsuiteは `test-database` 排他キーで直列にする。

本番アカウント・本番データの使用は禁止。PoCでは実Azure接続を必須にせず、DBなしのHono APIテストとプロジェクトが用意する統合テストを分離する。

---

## 13. 永続化・artifact・イベント

```text
.dle/
├── state.sqlite3
├── locks/
├── cache/{ast,graph}/
├── plans/<plan-id>.json
└── runs/<run-id>/
    ├── plan.json
    ├── environment.json
    ├── events.jsonl
    ├── result.json
    ├── junit.xml
    ├── tasks/<task-id>/stdout.log
    ├── tasks/<task-id>/stderr.log
    └── tasks/<task-id>/report.*
```

`.dle` をGit管理しない。ディレクトリは原則0700、機密を含み得るartifactは0600。ブラウザtraceやスクリーンショットにも秘密が含まれ得る。

SQLiteの最低限のテーブル：

- `schema_migrations`
- `workspaces`
- `plans`
- `runs`
- `tasks`
- `environment_leases`
- `artifacts`
- `duration_samples`

イベントは `schema_version / run_id / seq / timestamp / type / payload` を持つ。
run内のseqは単一writerで単調増加させる。性能計測のdurationはmonotonic clock、表示時刻はUTCとし、混同しない。

最低限のイベント：

`run_started`, `plan_created`, `selection_expanded`, `environment_preparing`,
`environment_ready`, `task_started`, `task_finished`, `cleanup_started`,
`cleanup_finished`, `run_finished`。

構造化イベントへログ全文を入れない。artifactを参照する。

runは成功終端レコードが確定するまで成功扱いにしない。クラッシュ後にRunningのまま残ったものはInterrupted相当として読み、再実行済みと見なさない。JSONはtemporary fileへ書いてatomic renameする。SQLiteとイベントの書込みが中途で止まるfixtureを用意し、再起動時の矛盾検出・cleanupを実装する。

---

## 14. CLI契約

すべて新規に実装するコマンド。`dle` は仮称。

```bash
# Read-only diagnostics. No installation, container deletion, or context switch.
dle doctor --format json

# Default local baseline is HEAD; be explicit for branch-wide changes.
dle plan --mode affected --base origin/main --format json
dle explain --plan .dle/plans/<plan-id>.json --suite api-unit

# Local, narrower scope: advisory result only.
dle test --mode affected --base HEAD --suite api-unit

# Run all configured suites.
dle verify --mode full

# Measure selection gaps while actually executing the full set once.
dle verify --mode shadow --base origin/main

# Execute in a newly owned Dev Container on Colima.
dle verify --mode full --environment colima

# Read stored result.
dle runs show <run-id> --format json

# Never delete unmanaged resources.
dle gc --dry-run
dle gc --apply
```

`plan` はGitとファイル読取りのみで作成できること。ランナーの設定ファイルを実行して検出する処理、パッケージインストール、Dev Container初期化は行わない。必要な能力が未確認ならその事実をplanに含め、実行preflightで検証・拡大する。

`test/verify` はplanを内部生成してから実行する。外部JSONを入力として実行する機能はv0.1では実装しない。将来実装する場合も、JSON内の任意argvをそのまま実行しない。

`--format json` のstdoutはJSONオブジェクト1個。進捗・子プロセス出力はstderr/artifactへ分ける。イベントstreamは別の `--format jsonl` で、JSONL契約に統一する。

### exit code

| code | 意味 |
|---|---|
| 0 | 指定scope/modeの検証成功、またはread-only操作成功 |
| 1 | テスト失敗・品質gate失敗・policyで禁止されたflaky/skip |
| 2 | 設定不正、trust不足、非対応能力、base不足、選択条件不正 |
| 3 | 実行基盤異常、timeout、report破損、入力変化、cleanup失敗 |
| 4 | `test/verify` で検証対象なし。`no_work`。合格とは別 |
| 130 | ユーザーキャンセル |

優先順位：ユーザーキャンセル > infrastructure error > configuration error >
test/policy failure > no_work > success。各taskの元の結果は消さない。

`plan` で対象0件を表示すること自体は正常処理なのでcode 0でよい。`verify full` で対象suite/実テストが0件なら通常は設定不備とし、no_workを品質gateに使わない。

---

## 15. 設定ファイル

正本は `dle.toml`。JSON/YAMLとの二重設定はv0.1では作らない。
未知キー、未知enum、重複suite ID、存在しないproject指定、循環するtask依存はエラー。

以下はサンプルモノレポ用の例である。パス、Vitest project名、既存の実行コマンドは対象repoに合わせる。`include` はsuiteの対象テスト領域、`inputs` はimport外も含めた入力宣言であり、未知依存を安全と断定するための設定ではない。

`whole_suite` は、差分があるaffected runではsuite全体を実行する。v0.1では `inputs` が非交差だからといってStorybook/E2Eを自動省略しない。狭いローカル検証には `--suite` を使う。

`project` がある場合adapterがランナーへ渡す。`workers`、report、selectionの予約済み引数をcommandへ二重に書かない。`include` と実際のランナー対象が矛盾する場合は、対象を黙って変えず設定不整合を報告する。

`environments.colima` を選んだrunではすべてのsuiteをその環境で実行する。環境内にBun/Nodeと依存が存在する必要がある。

```toml
schema_version = 1

[workspace]
root = "."
# Runtime outputs only. These are not proof that other ignored files are irrelevant.
exclude = [".git/**", ".dle/**", "**/node_modules/**"]
# Explicitly reviewed documentation-only paths. Keep empty by default.
non_test_inputs = []

[analysis]
language = "typescript_javascript"
granularity = "file"
parser = "oxc"
resolver = "oxc"
unknown_dependency = "all_enabled_suites"
global_inputs = [
  "dle.toml",
  "**/package.json",
  "**/bun.lock", "**/bun.lockb",
  "**/package-lock.json", "**/pnpm-lock.yaml", "**/yarn.lock",
  "**/tsconfig*.json",
  "**/vite.config.*", "**/vitest.config.*", "**/bunfig.toml",
  "**/playwright.config.*",
  ".storybook/**", "**/.storybook/**",
  ".devcontainer/**",
  "**/Dockerfile*", "**/compose*.yaml", "**/compose*.yml",
  "openapi/**", "schemas/**",
  "**/.env", "**/.env.*"
]

[execution]
default_environment = "local"
cpu_slots = 4
memory_budget_mib = 4096
max_parallel_suites = 2
default_timeout_seconds = 600
termination_grace_seconds = 5
cleanup_timeout_seconds = 30
fail_fast = false
max_log_bytes_per_stream = 16777216

[policy]
local_default_mode = "affected"
ci_default_mode = "full"
allow_result_cache = false
allow_ai_test_exclusion = false
fail_on_unexpected_skips = true
fail_on_flaky = true

[storage]
directory = ".dle"
history_retention_days = 14

[environments.local]
provider = "host"
# Logical scheduling reservations, not host resource isolation.
network = "inherit"

[environments.colima]
provider = "devcontainer_cli"
docker_context = "colima"
config = ".devcontainer/devcontainer.json"
lifecycle = "disposable"
allow_initialize_command = false
# Commands run after the reference CLI reports setup completion.
# This sample only checks tools; application readiness belongs in the project config.
readiness_commands = [["bun", "--version"], ["node", "--version"]]

[[suites]]
id = "api-unit"
runner = "bun"
cwd = "."
command = ["bun", "test"]
include = ["apps/api/**/*.test.ts", "apps/api/**/*.spec.ts"]
inputs = ["apps/api/**", "packages/**"]
selection = "file_graph"
always_run = false
timeout_seconds = 120
[suites.resources]
cpu_slots = 1
memory_mib = 512
exclusive = []

[[suites]]
id = "web-unit"
runner = "vitest"
cwd = "."
command = ["node", "./node_modules/vitest/vitest.mjs", "run"]
project = "unit"
include = ["apps/web/**/*.test.ts", "apps/web/**/*.test.tsx"]
inputs = ["apps/web/**", "packages/**"]
selection = "file_graph"
always_run = false
workers = 1
timeout_seconds = 180
[suites.resources]
cpu_slots = 1
memory_mib = 768
exclusive = []

[[suites]]
id = "storybook"
runner = "storybook_vitest"
cwd = "."
command = ["node", "./node_modules/vitest/vitest.mjs", "run"]
project = "storybook"
include = ["apps/web/**/*.stories.ts", "apps/web/**/*.stories.tsx"]
inputs = ["apps/web/**", "packages/**", ".storybook/**"]
selection = "whole_suite"
always_run = false
workers = 1
timeout_seconds = 300
[suites.resources]
cpu_slots = 1
memory_mib = 1024
exclusive = ["browser-budget"]

[[suites]]
id = "e2e"
runner = "playwright"
cwd = "."
command = ["node", "./node_modules/@playwright/test/cli.js", "test"]
include = ["tests/e2e/**/*.spec.ts"]
inputs = ["apps/**", "packages/**", "tests/e2e/**"]
selection = "whole_suite"
always_run = false
workers = 1
timeout_seconds = 600
[suites.resources]
cpu_slots = 1
memory_mib = 1536
exclusive = ["browser-budget", "test-database"]

[[suites]]
id = "typecheck"
runner = "command"
cwd = "."
command = ["bun", "run", "typecheck"]
include = []
inputs = ["apps/**", "packages/**", "**/tsconfig*.json"]
selection = "whole_suite"
always_run = true
timeout_seconds = 180
[suites.resources]
cpu_slots = 1
memory_mib = 768
exclusive = []
```

### 設定検証で必須のルール

- 有効なテストsuiteを少なくとも1つ要求する。commandだけの設定を「テスト検証」と称さない。
- `file_graph` を使えるのは対応確認済みBun/Vitestのみ。
- `project` の存在確認はrunner preflightで行う。static plan段階では未確認としてよい。
- v0.1では `allow_result_cache=true` / `allow_ai_test_exclusion=true` を拒否する。
- `cpu_slots` / memory / timeoutは正の値。予算超過は即時エラー。
- runnerごとにcwd・root・includeの解釈を契約テストで固定する。
- 作業対象外へ脱出するpath、NUL、artifact pathの衝突は拒否する。
- image名だけでなく、実際に使ったimage IDまたはdigestを実行結果に保存する。
- `.env` 等は入力として必要な場合でも内容をログ・JSONへ出力しない。
- `full` の対象登録を減らす設定変更もレビュー対象である。DLEは設定自体の妥当性を自動で証明しない。

`known_skips` は任意のsuite項目として定義する。既定は空配列。許可する場合は
`{project, file, title_path, expected_status}` の厳密なレコードを設定し、
fileはrepo-relative、title_pathは文字列配列、statusは`skipped`または`todo`。
ワイルドカードや「suite内のskipを全部許可」は設けない。
ランナーがcase identityを安定して提供できない場合、この例外機能は非対応とする。

---

## 16. JSON契約

公開出力はschema_version=1で固定する。別バージョンを黙って同じように解析しない。
pathはrepo-relativeの `/` 区切り、durationは整数milliseconds。
Planのfingerprint計算はキー順・集合順を固定したcanonical表現を用いる。
RunのIDや時刻はPlanIdに混ぜない。

### Planの形

以下は説明用のダミーID・件数であり、実行結果ではない。

```json
{
  "schema_version": 1,
  "plan_id": "blake3:<64-hex>",
  "workspace_id": "workspace-example",
  "snapshot_id": "blake3:<64-hex>",
  "baseline_commit": "<git-commit-id>",
  "config_digest": "blake3:<64-hex>",
  "mode": "affected",
  "scope": ["api-unit"],
  "assurance": "advisory",
  "changed_paths": ["apps/api/src/user.ts"],
  "suites": [
    {
      "suite_id": "api-unit",
      "requested_selection": {
        "kind": "files",
        "files": ["apps/api/src/user.test.ts"]
      },
      "effective_selection": {
        "kind": "files",
        "files": ["apps/api/src/user.test.ts"]
      },
      "reasons": [
        {
          "code": "transitive_dependency",
          "dependency_path": [
            "apps/api/src/user.test.ts",
            "apps/api/src/user.ts"
          ]
        }
      ],
      "capability_verification": "pending"
    }
  ],
  "diagnostics": []
}
```

`Files` のfileリストは非空・重複なし。suite/projectを含めて内部IDを解決する。
baseline_commitはfull実行で解析を必要としない場合nullを許可する。
snapshot_id/config_digestはfullでも必須。

### Resultの必須項目

```json
{
  "schema_version": 1,
  "run_id": "<uuid>",
  "plan_id": "blake3:<64-hex>",
  "mode": "affected",
  "scope": ["api-unit"],
  "assurance": "advisory",
  "outcome": "passed",
  "exit_code": 0,
  "input_validation": "unchanged",
  "environment": {
    "provider": "host",
    "os": "linux",
    "arch": "aarch64",
    "fingerprint": "blake3:<64-hex>",
    "tool_versions": {
      "bun": "<observed-version>"
    }
  },
  "suites": [
    {
      "suite_id": "api-unit",
      "outcome": "passed",
      "executed_files": ["apps/api/src/user.test.ts"],
      "counts": {
        "passed": 4,
        "failed": 0,
        "skipped": 0,
        "flaky": 0
      },
      "attempts": 1,
      "report": "tasks/api-unit/report.xml"
    }
  ],
  "metrics": {
    "planning_ms": 12,
    "environment_ready_ms": 2,
    "first_failure_ms": null,
    "verification_ms": 300,
    "cleanup_ms": 1,
    "total_ms": 315
  },
  "cleanup": {
    "outcome": "succeeded",
    "residual_resources": []
  }
}
```

上の時間は架空例。並列実行時のtotalは各suite時間の合計ではなくwall-clockで計測する。

`assurance` は `advisory / full_suite` とする。full/shadowで全登録suiteを正しく完了した場合のみfull_suite。これはアプリの正しさの数学的保証ではなく、登録された検証範囲の完了を表す。

commandアダプターのcountsはnullにし、テスト数を捏造しない。未知のfile attributionも空配列ではなく明示的なunknownを使う。その場合shadowの漏れ評価もsuite単位またはunknownで表示する。

### Eventの形

```json
{
  "schema_version": 1,
  "run_id": "<uuid>",
  "seq": 7,
  "timestamp": "2026-11-17T09:00:00Z",
  "type": "selection_expanded",
  "payload": {
    "suite_id": "api-unit",
    "from": "files",
    "to": "all",
    "reason_code": "unsupported_runner_version"
  }
}
```

errorのmessageへsecretや生の環境変数を含めない。machine-readableな`code`を必ず付ける。
実装時にはplan/result/event/configのJSON Schemaを生成または同梱し、実際のシリアライズ結果と検証する。

---

## 17. セキュリティ境界

この基盤は、信頼済みリポジトリの開発・テストを高速化するものである。
**コンテナを使っただけで、敵対的コードを安全に実行できるAI sandboxにはならない。**

### trust

- 初回のコマンド実行・ランナー設定ロード・Dev Container操作の前にworkspace trustを要求する。
- 承認はユーザー状態領域へ保存する。repo側のファイルだけで自己承認できないようにする。
- 実行設定、Dev Container、task command等の変更時はtrustを再確認する。
- CIは外部の信頼済みjob設定によって承認する。PR内の設定だけで権限昇格できる仕組みにしない。
- untrusted fork PRへsecret・Docker socket・privileged権限を与えない。
- `plan` は静的に読めるが、解析器にも資源上限を設ける。

### 権限

- Docker socketをテストコンテナへmountしない。
- privileged、host network、追加capability、ホストroot/homeの広範囲mount、SSH agent forwardingは既定拒否。
- v0.1の管理Dev Containerで必要性を評価できない危険設定は拒否する。安全設定へ黙って書き換えない。
- Docker管理権限は強い権限であり、通常のread-only APIと同等に扱わない。[S16]
- 実行時のネットワークはHostでは継承、Dev Containerでは選択設定に従う。v0.1が完全なegress制御を行うとは表示しない。
- secretをcommand argsへ埋め込まない。環境の受渡しはallowlistと明示的参照を使う。
- PATH/HOME/TMPDIR等の実行上必要な環境はpolicyで列挙する。cloud credential等を無条件継承しない。
- レポート表示のためにHTMLやスクリプトを自動実行しない。
- JUnit/XMLではDTD/外部実体を無効にする。JSON/XMLのサイズ・深さに上限を置く。
- symlinkをたどってrepo/artifact root外へ読書きしない。

### AIは将来レイヤー

将来AIが使うのは、まず`plan / explain / result`の読み取りのみとする。
テスト除外・required gate削除・secretアクセス・外部送信は許可しない。
修復は別branch/worktreeに差分として出し、承認と通常のfull検証を経る設計にする。
ログやソースに含まれる指示を実行権限として解釈しない。

---

## 18. 必須の受け入れテスト

実装者は以下をテストケースへ落とす。番号をテスト名またはテスト一覧に対応付ける。
通常系だけで完了にしない。

### 変更解析・グラフ

| ID | 入力・操作 | 期待 |
|---|---|---|
| G01 | Aをimportするテスト、Aのみ変更 | 該当テストを選択し経路を説明 |
| G02 | test→A→B、B変更 | 推移的に選択 |
| G03 | re-export/barrel越しの変更 | 関連テストを選択 |
| G04 | type-only import先変更 | 安全側で選択 |
| G05 | literal dynamic import / require | 対応profileで追跡 |
| G06 | 非literal dynamic import | 全有効suiteへ拡大 |
| G07 | alias解決失敗・構文解析失敗 | 影響なしではなくfallback |
| G08 | import元または先の削除 | 基準版グラフで利用側を保持 |
| G09 | rename | 旧/新path両方が計画に影響 |
| G10 | 新規untrackedテスト | inventoryへ追加し実行 |
| G11 | stagedとunstagedの混在 | 変更を取りこぼさない |
| G12 | lockfile/共有設定変更 | 全有効suiteへ拡大 |
| G13 | 空白・日本語・改行を含むGit path | NUL区切りで誤解析しない |
| G14 | 非UTF-8 path | 明示的非対応。lossy変換しない |
| G15 | tsconfig/package条件変更 | resolver cache失効 |
| G16 | 未変更importの解決先が新規ファイル追加で変化 | 全解決関係を再計算 |
| G17 | repo外symlink/未対応submodule | 明示的エラーまたは保守的fallback |
| G18 | 同じ入力を順不同で列挙 | canonical planが一致 |

### 選択・結果

| ID | 入力・操作 | 期待 |
|---|---|---|
| P01 | Files空集合 | 型/検証で拒否 |
| P02 | コード変更なのに関連テスト0件 | 全有効suiteへ拡大 |
| P03 | 変更なし | no_work。passedとは別 |
| P04 | global/non_test入力の重複 | global優先 |
| P05 | unsupported runner filter | full suiteへ拡大 |
| P06 | shadow | fullを1回実行し、候補漏れを記録 |
| P07 | CIでaffectedだけ、または--suite限定 | 品質gateとして拒否 |
| P08 | ランナーexit 0だがreport欠落 | infrastructure_error |
| P09 | 実行予定ファイルがreportにない | InvalidReport |
| P10 | 新規skip/todo、retry後pass | policy判定・flaky保持 |
| P11 | 同名testを異なるprojectで実行 | 結果を衝突させない |
| P12 | commandアダプター成功 | テスト件数unknown |
| P13 | plan作成後に入力変更 | 古いplanを無効化 |
| P14 | reportが空/壊れたXML/XXEを含む | 安全に失敗 |
| P15 | 前回reportが残っている | 新runの結果として読まない |
| P16 | 計画へ未知依存やエッジを追加 | 選択集合が縮まらない |

### 実行・環境

| ID | 入力・操作 | 期待 |
|---|---|---|
| R01 | task資源が全予算より大きい | 即時エラー。無限待ちしない |
| R02 | 排他DBキーを共有するsuite | 同時実行しない |
| R03 | runner workersと外側並列 | 合計予約予算を超えない |
| R04 | 大量stdout/stderr | 詰まらず、メモリ無制限増大なし |
| R05 | Host timeout、子プロセスあり | group停止・回収を確認 |
| R06 | SIGINT | 新規投入停止→cleanup→130 |
| R07 | コンテナexecクライアントを停止 | 専有containerも停止・後始末 |
| R08 | cleanup自体失敗 | runを完全成功にしない。残存ID記録 |
| R09 | 利用者の編集用Dev Container存在 | attach・停止・削除しない |
| R10 | ラベル不一致 | cleanupを拒否 |
| R11 | postCreateが遅い | 完了前にテストを始めない |
| R12 | initializeCommandあり・未承認 | Hostで実行しない |
| R13 | readiness失敗 | 環境準備失敗、テストを開始しない |
| R14 | contextとDOCKER_HOSTが矛盾 | 明示した接続先以外に触れない |
| R15 | 動作中にDLEがクラッシュ | 次回に中断run・所有資源を検出 |
| R16 | secretを含む環境 | JSON/ログへ平文出力しない |
| R17 | 同じworkspaceで二重実行 | lease/ファイルの競合を防ぐ |
| R18 | Playwright setup/project dependencies | 無断で省略しない |

さらにproperty-based testで、集合の単調性、グラフ循環、duplicate edge、順序独立性、path正規化を確認する。

---

## 19. 品質ゲートとCI

DLE自身のCIで最低限実行する。

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

これに以下を追加する。

- Rust依存方向の検査。domain/applicationからadapterへ依存させない。
- 型・JSON Schema・TOML例の整合性。
- 各runnerの実プロセスを使う契約テスト。
- Hostの終了・キャンセル・子孫process回収テスト。
- Linux上のDocker + Dev Container CLI統合テスト。
- macOS + Colimaの実機検証。実行不能なら「未実行」と明記する。
- pinned runner versionsのcompatibility matrix。
- clean checkoutからのサンプル実行。
- 後始末で他のコンテナ/volumeに触れないことの検査。

通常の `cargo test` で外部依存テストを無言でskipして、全環境対応済みと称さない。
実機を必要とするものは明示した別job/コマンドに分離し、実行有無をレポートする。

導入先プロジェクトでは当面、従来full suiteとDLE full/shadowを比較する。
DLE affectedだけを必須チェックに置き換える変更は今回の範囲外。

---

## 20. ベンチマークと成功基準

### 測るもの

- 差分取得、AST解析、resolver、グラフ探索、plan作成。
- containerのbuild/create/ready、lifecycle完了。
- queue待ち、ランナー起動、実テスト、cleanup。
- first failureまでの時間、全検証完了までのwall-clock。
- DLE自身とランナーのCPU/peak RSS。
- 選択予定/実行済みファイル数、fallback率、shadow gap候補。
- cold/warm image、cold/warm dependency cacheを分ける。

### 比較条件

同じcommit、lockfile、image、OS/arch、CPU/RAM、runner worker数、ブラウザ、DB初期状態で比較する。ネットワーク取得を伴う実行は別系列にする。v0.1でWarm Poolを実装していないのに「warm environment」と呼ばない。温まっているのがimage cacheだけならそう記載する。

原則ウォームアップ後に30回程度繰り返してp50/p95を記録する。計測回数が少ない場合はその制約を記す。

### 比較対象

1. 既存runnerを直接full実行。
2. DLE full実行：オーケストレーション自体の追加負荷を測る。
3. DLE affected：同じ変更で選択と全実行を別の初期化済み環境で比較する。
4. HostとColimaの環境準備込み比較。
5. v0.2以降でのみWarm Pool/追加runtime比較。

### 合格条件

- 必須fixtureで予定された影響テストの取りこぼし0件。
- 不確実性のfixtureでは必ずfallbackまたは明示的エラー。
- キャンセル後に確認対象のmanaged process/containerが残らない。
- base/dirty/inputを正しく識別し、誤ったpass再利用をしない。
- 実際の性能をrawデータ付きで記録し、改善しない場合は原因を報告する。
- 全体性能向上と機能削減による見かけの向上を区別する。

改善倍率は未測定なので仕様に約束しない。Rust化による効果と、実行対象を減らした効果を分けて報告する。

---

## 21. 技術選定

| 役割 | 初期候補・方針 |
|---|---|
| CLI | clap |
| 非同期/プロセス | Tokio、tokio-utilのキャンセル機構 |
| JSON/TOML | serde / serde_json / toml。configはunknown field拒否 |
| エラー | thiserror。CLI最外層のみcontext付与を許可 |
| TS/JS | Oxc parser / oxc_resolver |
| グラフ | domainのadjacency表で開始。必要ならpetgraphをadapter内部に利用 |
| fingerprint | BLAKE3、canonical serialization |
| history | rusqlite + SQLite。blocking IOは実行executorを塞がない |
| glob | globset等。Git ignoreとsuite inputの意味を混同しない |
| XML | 安全に設定したXML parser。DTD/外部実体禁止 |
| ログ | tracing。OpenTelemetry exportは後続 |
| テスト | Rust unit/integration、proptest、golden fixtures |
| Docker接続 | v0.1は公式CLI/Engine CLIへ委譲。Bollard直接利用は必要時のみ |

ライブラリのバージョンは本仕様では固定値を捏造しない。初回実装時に互換性を確認して正確な版をlockし、その後は無断更新しない。

JSON Schema生成方式も一つに統一し、型とschemaが乖離しないテストを設ける。
この候補リストを理由に、不要な依存を全部入れない。

---

## 22. 実装ロードマップと各段階の完了条件

### M0：リポジトリ調査・契約・骨格

作るもの：
4クレートworkspace、CLI引数、設定型、domain型、エラー、fake ports、
architecture test、ADR-001（scopeと依存方向）。

完了条件：
既存コードを壊さずcargo check/testが成功する。
invalid config、Files空、path traversalがテストで拒否される。
未実装機能を成功で返すstubがproduction経路にない。

### M1：差分・グラフ・plan/explain

作るもの：
Git snapshot、基準/現在グラフ、Oxc解析、逆依存探索、fallback、
deterministic plan JSON、explain。

完了条件：
G01〜G18、P01〜P07/P13/P16相当が通る。
`plan` でソースコードやランナー設定を実行しない。
full modeでbaseを不要にできる。

### M2：Host実行・Bun/Vitest・結果

作るもの：
HostRuntime、process group、timeout/cancel、Bun/Vitest adapters、
資源予約、raw report→統合JSON/JUnit。

完了条件：
成功・assertion失敗・report破損・0件・skip・flaky・cancelを区別する。
要求ファイルを実際に実行したことを検査する。
R01〜R06、P08〜P15を満たす。
少なくとも一つの実TS fixtureで `dle test --mode affected` が動く。

### M3：Storybook/Playwright・全体検証

作るもの：
Storybook-Vitest adapter、Playwright adapter、command gate、
full/shadow、project identity、互換性fixture。

完了条件：
Storybookはwhole project、Playwrightはwhole suiteで実行する。
project dependencies・setup・MSWとの責務を維持する。
API変更→unit/API、UI→component/E2Eの検証範囲を混同しない。
リポジトリの既存full結果と比較できる。

### M4：Colima・Dev Containerの基準実装

作るもの：
DevContainerCliRuntime、明示context、所有ラベル、readiness、
lifecycle待機、disposable cleanup、再起動後の残存資源検知。

完了条件：
既存の開発用コンテナに一切手を付けない。
R07〜R17を満たす。
macOSの実機がなければLinux統合までを実行し、Colimaは未検証と明示する。
「CLIを呼べた」だけでは完了にしない。

### M5：運用・計測・引き継ぎ

作るもの：
SQLite履歴、artifact保持、gc dry-run/apply、benchmark、
サンプル、README、compatibility、JSON schemas、既知制約。

完了条件：
clean checkoutから手順通りに動く。
実行したテスト/未実行テストを分けて報告できる。
raw benchmarkと環境情報を残す。
全受け入れ条件と実装/テストの対応表がある。

### v0.2以降

優先順は、計測結果から決める。

1. 同一trust domain内のResettable環境・Warm Pool。
2. 互換性を維持した環境準備・依存インストールの削減。
3. 独立leaseでのコンテナ並列・テストDB分離。
4. 対応runnerの安全なケース単位選択。
5. 観測データによる優先度調整・flaky分析。
6. Kiriアダプターの互換性/性能検証。
7. GUI / AI診断 / チーム共有。
8. soundな入力モデルを定義できた範囲だけテスト結果cache。

MVPの遅さが単なるCLI生成コストでない限り、Dev Container CLIのRust再実装へ進まない。

---

## 23. Claudeの作業・報告規約

最初に次を実施する。

1. 現在のworkspace、git status、既存のルール、ランナー設定、lockfileを読む。
2. 本仕様との差分と既存ファイルへの影響を列挙する。
3. M0から着手し、以後は小さな垂直スライスで進める。
4. fixture/契約テストを先に用意し、正常系と異常系を一緒に実装する。
5. 成功した実行コマンド・失敗・実行不能を区別して記録する。

禁止事項：
未検証ベンチマークの作成、実行していないテストの成功報告、
TODOをpassで埋める、既存テストの削除/skipでgreen化、
production secretの要求、無断のcrate/API版更新、勝手なGUI/AI実装、
ユーザーの作業中変更へのreset/上書き。

不明点の扱い：
安全性に関わるものは保守的にエラーまたはfullへ拡大する。
実装都合の小さな選択はADRへ記録して進める。
対応範囲を減らす必要がある場合は明示し、別機能が動いたことをもって対応済みとしない。

各マイルストーンの完了報告は次の形式とする。

```text
Milestone:
Implemented:
Changed files:
Tests executed (exact commands):
Tests not executed (reason):
Known limitations:
Measured performance (or "not measured"):
Next milestone:
```

この仕様を読んだ後、設計案を繰り返すだけで終わらず、
実装可能な環境がある場合はM0から実装・検証を開始する。

---

## 24. 一次情報と設計判断の区別

本文の「必須」「禁止」「v0.1」はDLEの設計判断である。
参照先がDLEの安全性や性能を保証しているわけではない。
公式仕様の説明と本仕様の追加制約を混同しない。

確認日：2026-11-17。実装時に固定する依存バージョンでも再確認する。

- [S1] Vitest CLI：`https://vitest.dev/guide/cli`
- [S2] Bun reporters：`https://bun.com/docs/test/reporters`
- [S3] Bun test discovery：`https://bun.com/docs/test/discovery`
- [S4] Playwright CLI：`https://playwright.dev/docs/test-cli`
- [S5] Storybook Vitest addon：`https://storybook.js.org/docs/writing-tests/integrations/vitest-addon`
- [S6] Dev Container CLI（公式参照実装）：`https://github.com/devcontainers/cli`
- [S7] Git diff：`https://git-scm.com/docs/git-diff`
- [S8] Git ls-files：`https://git-scm.com/docs/git-ls-files`
- [S9] Tokio process Command：`https://docs.rs/tokio/latest/tokio/process/struct.Command.html`
- [S10] Docker build cache：`https://docs.docker.com/build/cache/optimize/`
- [S11] Oxc parser：`https://oxc.rs/docs/guide/usage/parser`
- [S12] Oxc resolver：`https://docs.rs/oxc_resolver/latest/oxc_resolver/`
- [S13] Playwright parallelism：`https://playwright.dev/docs/test-parallel`
- [S14] Dev Container JSON/lifecycle reference：`https://github.com/devcontainers/spec/blob/main/docs/specs/devcontainerjson-reference.md`
- [S15] Dev Container CLIの実際の引数定義：`https://github.com/devcontainers/cli/blob/main/src/spec-node/devContainersSpecCLI.ts`
- [S16] Docker Engine security：`https://docs.docker.com/engine/security/`
- [S17] Vitest reporters：`https://vitest.dev/guide/reporters`
- [S18] Playwright reporters：`https://playwright.dev/docs/test-reporters`

以上。
