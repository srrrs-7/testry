# 受け入れ条件とテストの対応表

仕様 18 章の ID と、それを検証しているテストの対応。**「検証済み」は実際に実行して通ったものだけ**を指す
（実行コマンドは README の「品質ゲート」）。実装が無いものは「未着手」と書き、成功扱いにしない。

テストの場所の略記:

- `domain` = `crates/testry-domain/src/*.rs` の単体・property テスト
- `app` = `crates/testry-application/tests/planning.rs`（fake ports）
- `extract` / `vfs` / `oxc` = `crates/testry-adapters/src/analysis/`
- `git` = `crates/testry-adapters/tests/git_snapshot.rs`（実 Git）
- `cli` = `crates/testry-cli/tests/cli_plan.rs`（実バイナリ + 実 Git）
- `exec` = `crates/testry-application/tests/execution.rs`（実行ユースケース、fake ports）
- `results` / `policy` / `scheduler` = `crates/testry-application/src/` の単体テスト
- `contract` = `crates/testry-adapters/tests/runner_contract.rs`（**実 Bun 1.4.4 / Vitest 5.1.0 / Storybook 10.6.7 / Playwright 1.65.1** を、Chromium つきで起動）
- `process` = `crates/testry-adapters/tests/process.rs`（実プロセス・実シグナル）
- `run` = `crates/testry-cli/tests/cli_run.rs`（実バイナリ + 実 Git + 実ランナー）
- `browser` = `crates/testry-cli/tests/cli_browser.rs`（実バイナリ + 実 Git + Storybook / Playwright + 実ブラウザ）
- `pw_json` = `crates/testry-adapters/src/reports/playwright_json.rs`、`playwright` / `storybook` = `crates/testry-adapters/src/runners/` の単体テスト（実レポートの記録 fixture を使用）
- `history` / `gc` / `run_files` / `lock` / `trust` = `crates/testry-adapters/src/storage/` の単体テスト（実ファイル・実 SQLite）

## 変更解析・グラフ（M1）

| ID | 状態 | テスト |
|---|---|---|
| G01 | 検証済み | domain `g01_direct_dependency_is_selected_and_explained` / oxc `g01_g02_…` / cli `g01_changed_module_selects_its_test_and_explains_the_path` |
| G02 | 検証済み | domain `g02_transitive_dependency_is_selected` / cli `g02_g03_transitive_change_through_package_barrel_and_alias` |
| G03 | 検証済み | extract `g03_re_exports_are_dependencies` / oxc `g03_barrel_re_exports_are_followed` / cli `g02_g03_…` |
| G04 | 検証済み | extract `g04_type_only_imports_are_dependencies` / oxc `g04_type_only_imports_resolve_to_declarations` |
| G05 | 検証済み | extract `g05_literal_dynamic_import_and_require` / oxc `g05_…` |
| G06 | 検証済み | extract `g06_…` / oxc `g06_…` / domain `g06_g07_graph_uncertainty_widens_all_suites` / cli `g06_g07_uncertainty_widens_every_suite` |
| G07 | 検証済み | extract `g07_syntax_errors_are_parse_failures` / oxc `g07_unresolved_alias_and_parse_failure_are_diagnostics` / cli `g06_g07_…` |
| G08 | 検証済み | domain `g08_deleted_dependency_keeps_its_consumers` / oxc `g08_baseline_graph_keeps_deleted_and_changed_dependencies` / git `g08_deleted_files_stay_in_the_baseline` / cli `g08_g09_…` |
| G09 | 検証済み | git `g09_rename_reports_old_and_new_path` / cli `g08_g09_deleted_and_renamed_dependencies_keep_their_consumers` |
| G10 | 検証済み | git `g11_…`（未追跡を列挙）/ cli `g10_new_untracked_test_is_selected` |
| G11 | 検証済み | git `g11_default_base_is_head_and_covers_staged_unstaged_and_untracked` |
| G12 | 検証済み | domain `g12_p04_…` / app `p04_g12_global_inputs_widen_every_suite` / cli `g12_global_inputs_widen_every_suite` |
| G13 | 検証済み | git `g13_unusual_paths_are_parsed_exactly`（空白・日本語・改行・タブ・引用符・角括弧）/ oxc `paths_with_spaces_and_japanese_resolve` |
| G14 | 検証済み | domain `rejects_non_utf8_without_lossy_conversion` / git `g14_non_utf8_paths_are_rejected` |
| G15 | 検証済み（注1） | oxc `g15_resolution_follows_tsconfig_changes` / cli `g12_…`（tsconfig 変更は global input） |
| G16 | 検証済み（注1） | oxc `g16_resolution_is_recomputed_when_a_file_is_added` / oxc `ambiguous_siblings_are_all_dependencies` |
| G17 | 検証済み | vfs `g17_opaque_paths_look_missing_and_are_recorded` / oxc `g17_symlinks_and_submodules_are_not_followed` / git `g17_symlinks_are_never_followed` / git `g17_submodules_are_reported_and_not_analyzed` |
| G18 | 検証済み | domain `g18_canonical_plan_is_independent_of_enumeration_order` / app `g18_plan_is_identical_for_reordered_inputs` / cli `g18_plans_are_deterministic` |

注1: 解決結果のディスクキャッシュは未実装で、毎回すべて再計算する。「キャッシュの失効」は
検証対象のキャッシュが存在しないという形で満たしている。キャッシュを導入する際は G15 / G16 に
キャッシュ有りのテストを追加する必要がある。

## 選択・結果

| ID | 状態 | テスト / 備考 |
|---|---|---|
| P01 | 検証済み | domain `p01_empty_file_set_is_rejected` / `p01_empty_file_list_is_rejected_on_deserialization` |
| P02 | 検証済み | domain `p02_…` / app `p02_…` / cli `p02_unrelated_change_widens_every_suite` |
| P03 | 検証済み | domain `p03_…` / app `p03_no_changes_is_no_work_except_for_always_run` / exec `p03_no_work_prepares_no_environment_and_is_not_a_pass` / run `p03_no_changes_exit_with_four`（exit 4、環境もランナーも起動しない） |
| P04 | 検証済み | domain `g12_p04_global_input_widens_all_suites_and_wins_over_non_test_inputs` / app `p04_g12_…` |
| P05 | 検証済み | app `p05_unsupported_runner_filter_widens_to_the_whole_suite` / exec `p05_unverified_runner_version_widens_to_the_whole_suite` / vitest `p05_file_selection_needs_a_verified_version`。版は run ごとに実コマンドで確認する（run `verify_full_runs_every_suite_with_the_real_runners` が `tool_versions` を照合） |
| P06 | 検証済み | app `p06_…` / exec `p06_shadow_runs_everything_once_and_records_gap_candidates` / results `p06_shadow_records_failures_outside_the_candidate` / run `p06_shadow_records_a_failure_outside_the_affected_candidate` |
| P07 | 検証済み | app `p07_ci_rejects_affected_and_scope_limits_before_any_io` / cli `p07_ci_rejects_affected_and_scope_limits` / 設定 `p07_ci_default_mode_must_be_a_gate` / cli 単体 `p07_gate_rules_apply_to_test_and_verify_but_not_to_plan` |
| P08 | 検証済み | exec `p08_exit_zero_without_report_is_an_infrastructure_error` / bun `p08_p14_missing_and_broken_reports` / vitest `p08_a_missing_report_is_an_infrastructure_error` / contract `p08_bun_without_any_test_is_an_infrastructure_error` |
| P09 | 検証済み | results `p09_expected_file_missing_from_the_report` / exec `p09_planned_file_missing_from_the_report` / contract `p09_bun_file_missing_from_the_report_is_not_a_pass`。exit 0 なら infrastructure_error、exit ≠ 0 なら failed（ADR-0004 の 6） |
| P10 | 検証済み（注2） | policy `p10_unexpected_skips_fail_the_suite` / `p10_flaky_tests_are_kept_and_fail_by_policy` / `an_expected_failure_needs_its_own_approval` / exec `p10_skips_and_flaky_tests_follow_the_policy` / contract `p10_vitest_retried_tests_are_flaky_and_statuses_are_kept` / `playwright_runs_the_whole_suite_and_keeps_statuses_apart`（skip・fixme・flaky・expected failure）/ run `p10_an_unexpected_skip_fails_the_run` / browser `p10_an_expected_failure_needs_an_exact_approval` |
| P11 | 検証済み | domain `p11_same_path_in_two_projects_does_not_collide` / exec `p11_same_file_in_two_projects_does_not_collide` / junit `p11_same_test_in_two_suites_does_not_collide` / `projects_of_one_invocation_and_expected_failures_stay_visible` / pw_json `p11_the_project_is_part_of_the_identity_of_a_case` / policy `a_skip_is_approved_for_the_project_the_case_ran_in` / contract `p11_vitest_projects_are_run_separately` / browser `verify_full_…`（JUnit の `e2e[chromium]:…`） |
| P12 | 検証済み | results `p12_command_results_have_unknown_counts` / exec `p12_command_gate_has_unknown_counts` / contract `p12_command_adapter_runs_the_fixed_argv` / run `verify_full_…`（`counts: null`） |
| P13 | 検証済み | app `p13_a_plan_is_invalid_for_changed_inputs` / git `p13_snapshot_id_is_stable_and_follows_content` / exec `p13_inputs_changed_before_execution_invalidate_the_run` / `p13_inputs_changed_during_execution_invalidate_the_result` / run `p13_inputs_changed_during_the_run_invalidate_it`（実テストが入力を書き換える） |
| P14 | 検証済み | bun_junit `p14_doctype_and_entities_are_rejected` / `p14_unusable_reports_are_rejected` / results `p14_unusable_reports_are_infrastructure_errors` / exec `p14_unusable_reports_fail_safely` / run_files `only_a_regular_report_file_is_read` |
| P15 | 検証済み | contract `p15_a_stale_report_is_not_reused` / run_files `a_run_directory_is_private_ignored_by_git_and_never_reused` / run `each_run_has_its_own_artifacts` |
| P16 | 検証済み（範囲つき） | domain `p16_adding_unknown_dependencies_never_shrinks` / `p16_adding_edges_never_shrinks_a_graph_derived_selection` / graph `adding_edges_never_shrinks_dependents`。範囲は ADR-0003「単調性」参照 |

注2: Bun の per-test `{ retry: n }` は JUnit XML に現れないため、Bun では「retry 後に通った」を検出できない
（suite の command に `--retry` を書くことは設定エラーにしている）。flaky の検出を実機で確認したのは Vitest と Playwright。
Storybook の `tags: ["!test"]` による story の除外はレポートに現れず、検出できない（ADR-0005 の 1）。

## 実行・環境

| ID | 状態 | 備考 |
|---|---|---|
| R01 | 検証済み | domain `r01_…` / 設定 `r01_suite_larger_than_the_budget_is_rejected` / scheduler `r01_oversized_task_is_rejected_before_anything_starts` |
| R02 | 検証済み | domain `r02_exclusive_keys_are_never_shared` / scheduler `r02_exclusive_keys_serialize_tasks` / exec `r02_r03_suites_respect_exclusive_keys_and_the_budget` |
| R03 | 検証済み（注3） | domain `r03_workers_must_match_the_cpu_reservation` / scheduler `r03_reservations_stay_within_the_budget` / exec `r02_r03_…` |
| R04 | 検証済み | process `r04_large_output_is_drained_and_stored_up_to_the_limit` |
| R05 | 検証済み | process `r05_timeout_stops_the_whole_process_group` / `r05_sigterm_is_escalated_to_sigkill` / `r05_a_timeout_stops_processes_that_left_the_group`（別セッションへ出て SIGTERM を無視するプロセス）/ run `r05_a_timeout_stops_the_runner_and_its_children` / contract `r05_playwright_timeout_leaves_no_browser_or_server_behind`（実ブラウザと `webServer`） |
| R06 | 検証済み | scheduler `r06_cancellation_stops_new_launches` / exec `r06_cancellation_stops_new_suites_and_still_cleans_up` / process `r06_cancellation_stops_the_process_group` / run `r06_sigint_cancels_the_run_and_cleans_up`（実 SIGINT → exit 130） |
| R07 | 対象外 | Dev Container provider を実装しない（利用者の決定、ADR-0006）。testry はコンテナを作らず、exec クライアントも持たない |
| R08 | ユースケースのみ検証済み | results `r08_failed_cleanup_prevents_a_passed_run` / exec `r08_failed_cleanup_is_never_a_passed_run`（fake runtime）。host provider は停止を確認できなかったプロセスグループを残存資源として報告する。コンテナの cleanup 失敗は M4 |
| R09〜R12 | 対象外 | 同上。testry はどのコンテナにも attach・停止・削除をせず、`devcontainer.json` も lifecycle コマンドも扱わない |
| R13 | ユースケースのみ検証済み | exec `r13_environment_preparation_failure_starts_no_test`（fake runtime）。host provider には readiness コマンドが無い（環境の用意は testry の外側の責務。ADR-0006） |
| R14 | 対象外 | 同上。testry は Docker に接続しない |
| R15 | 検証済み（注4） | 中断 run の検出: run_files `a_run_that_stopped_while_writing_its_result_is_interrupted` / `the_history_is_repaired_after_a_stop_at_every_point` / history `reconciling_follows_the_run_directories` / run `runs_show_reads_finished_and_interrupted_runs`。残ったプロセスの検出と回収: process `r15_processes_of_a_run_can_be_found_and_stopped_after_testry_is_gone` / run `r15_a_run_whose_testry_was_killed_is_found_and_cleaned_up`（実行中の testry を SIGKILL し、残ったランナーを `gc` で回収）。ロックはプロセス終了で OS が解放する |
| R16 | 一部検証済み | 子プロセスへ渡す環境変数は許可リストのみ: host `only_allowlisted_variables_are_inherited` / process `the_environment_is_exactly_what_was_given`。testry は秘匿値を受け取る手段を持たない（任意の環境変数を suite に渡せない）ので、ログ・JSON 内のマスクは未実装のまま |
| R17 | 検証済み | lock `r17_only_one_run_holds_the_workspace` / run `r17_a_second_run_in_the_same_workspace_is_refused` |
| R18 | 検証済み | playwright `r18_a_project_is_selected_without_dropping_its_dependencies`（`--no-deps` は設定エラー）/ pw_json `r18_tests_behind_a_failed_setup_project_did_not_run` / contract `r18_playwright_setup_failure_is_not_a_pass` / `r18_playwright_project_selection_keeps_dependencies` / browser `r18_a_failing_setup_project_is_never_skipped_over` / `verify_full_…`（setup project の実行を確認）。`globalSetup` / `webServer` には触れない設計で、`webServer` は R05 の契約テストで実際に起動している |

注3: Vitest には `--maxWorkers=<workers>` を渡す。`workers` の指定は必須で、予約した `cpu_slots` を超える設定は拒否する。
Bun には worker を制御する手段が無いため、bun suite に `workers` や `--parallel` を書くことを設定エラーにしている。
予約はスケジューリング上の予算であり、OS による強制ではない。

## 仕様 6 章・13 章・14 章・17 章の個別要件

| 要件 | 状態 | テスト |
|---|---|---|
| 作業ツリー・index・ref を変更しない | 検証済み | git `the_work_tree_index_and_refs_are_left_untouched` / cli `plan_leaves_the_repository_untouched_apart_from_its_ignored_state` / run `verify_full_…`（実行後も `git status` が空） |
| 外部 diff / textconv を起動しない | 検証済み | git `external_diff_and_textconv_programs_are_never_started` |
| base 不明を空 diff にしない・fetch しない | 検証済み | git `an_unknown_base_is_an_error_and_nothing_is_fetched` / `unrelated_histories_have_no_merge_base` / cli `an_explicit_base_covers_the_whole_branch` |
| コンフリクト中の index はエラー | 検証済み | git `a_conflicted_index_is_an_error` |
| `plan` はソース・ランナー設定を実行しない | 検証済み | cli `plan_executes_no_project_code_and_starts_no_runtime` |
| `full` は base 不要 | 検証済み | app `full_mode_needs_neither_a_baseline_nor_an_analysis` / cli `full_mode_needs_no_baseline_and_works_without_commits` |
| `--format json` の stdout は JSON 1 個 | 検証済み | cli / run の各テスト（stdout 全体を 1 値としてパース） |
| `--format jsonl` は seq が 1 ずつ増えるイベント列 | 検証済み | run `events_are_ordered_and_complete` / run_files `events_are_numbered_by_the_single_writer_in_file_order` |
| 未実装の機能が成功を返さない | 検証済み | cli `execution_is_refused_without_trust_and_nothing_is_started`/ exec `a_suite_without_adapter_is_refused_before_anything_runs` / run `unsupported_environments_are_refused`（Dev Container） |
| 改変・他バージョンの plan を拒否 | 検証済み | domain `tampered_plans_are_rejected` / cli `explain_rejects_modified_and_foreign_plan_documents` |
| symlink 経由で状態を読み書きしない | 検証済み | adapters `refuses_to_write_through_a_symbolic_link` / run_files `a_link_in_place_of_the_run_directory_is_refused` / `a_terminal_record_that_is_a_link_is_not_followed` / lock `a_symbolic_link_in_place_of_the_storage_is_refused` |
| artifact の権限（0700 / 0600）と run ディレクトリ外への参照拒否 | 検証済み | run `verify_full_…` / run_files `artifact_references_cannot_leave_the_run_directory` / process `output_files_are_private` |
| 成功の終端レコードが無い run を成功扱いしない | 検証済み | run_files `stored_runs_are_read_by_their_terminal_record` / `a_run_that_stopped_while_writing_its_result_is_interrupted` / cli 単体 `an_interrupted_run_is_never_summarized_as_a_result` |
| workspace trust（未承認・設定変更後は実行しない） | 検証済み | exec `nothing_runs_without_trust_for_exactly_this_configuration` / trust `approvals_are_per_workspace_and_per_configuration` / `trust_cannot_be_stored_inside_the_workspace` / `a_pinned_digest_from_the_environment_is_validated` / run `nothing_runs_until_the_configuration_is_trusted` / `a_job_definition_can_pin_the_trusted_configuration_digest` |
| コンテナ外では既定で実行しない（ADR-0002 / 0004） | 検証済み | host `execution_outside_a_container_is_refused_by_default` / `execution_outside_a_container_needs_the_explicit_opt_out` |
| シェルを介さず argv で起動する | 検証済み | process `runs_in_the_given_directory_without_a_shell_for_arguments` |
| `.only` で他のテストが黙って消えない | 検証済み | contract `bun_focused_tests_fail_the_suite` / `vitest_focused_tests_fail_the_suite` / `playwright_focused_tests_fail_the_suite` |
| Storybook は Vitest の 1 project 全体として実行する（M3） | 検証済み | storybook `runs_exactly_one_vitest_project_without_telemetry` / `a_file_selection_becomes_the_whole_project` / contract `storybook_runs_the_stories_project_as_a_whole` / `storybook_story_file_that_cannot_be_loaded_is_not_a_pass` / browser `verify_full_…` |
| Playwright は suite 全体として実行する（M3） | 検証済み | playwright `a_file_selection_becomes_the_whole_suite` / `commands_that_narrow_hide_or_own_the_run_are_rejected` / contract `playwright_runs_the_whole_suite_and_keeps_statuses_apart` / `p08_playwright_without_any_test_is_not_a_pass` / `playwright_file_that_cannot_be_loaded_is_a_failure` |
| ランナー設定による絞り込みを明示し full_suite を名乗らない | 検証済み | results `a_suite_narrowed_by_its_runner_configuration_is_not_full_suite` / pw_json `narrowing_settings_of_the_runner_are_reported` / contract `playwright_narrowing_configuration_is_reported` / browser `a_narrowing_playwright_configuration_is_not_a_full_run` |
| suite の command の矛盾は、最初の suite を始める前に設定エラーにする | 検証済み | exec `a_command_its_adapter_refuses_stops_the_run_before_any_suite_starts` / browser `a_playwright_command_that_narrows_the_suite_is_refused` |
| API 変更は unit を絞り、ブラウザ suite は絞らず省略もしない（M3） | 検証済み | browser `an_api_change_narrows_unit_tests_but_never_the_browser_suites` / `a_failing_story_fails_only_the_stories_suite` |
| 統合結果がランナーを直接実行した結果と一致する（M3） | 検証済み（fixture の範囲） | browser `the_unified_result_agrees_with_the_runner_run_directly`。実リポジトリの既存 CI 結果との比較は未実施 |
| プロセスグループの外に出たプロセスを止める | 検証済み | process `processes_that_left_the_group_are_stopped_after_a_normal_exit` / `the_marker_of_one_command_does_not_reach_another_command` |
| ランナーの成果物を作業ツリーに書かせない（Playwright） | 検証済み | contract `playwright_runs_the_whole_suite_…`（`test-results/` が無い）/ browser `verify_full_…`（実行後も `git status` が空） |
| 依存グラフが実ランナーの解決と一致する（fixture の範囲） | 検証済み | contract `the_dependency_graph_contains_what_each_runner_really_loads` |
| `compatibility.toml` がコード・イメージ・fixture の依存と一致する | 検証済み | `crates/testry-cli/tests/compatibility.rs`（5 件） |
| 依存方向・unsafe 禁止 | 検証済み | `crates/testry-cli/tests/architecture.rs`（9 件） |
| SQLite の履歴（仕様 13 章の 8 表、0600、スキーマ版の確認） | 検証済み | history `a_new_history_is_private_and_has_every_table` / `a_history_of_a_newer_testry_is_refused_and_left_untouched` / `something_else_in_place_of_the_history_is_refused` / `a_run_is_running_until_its_terminal_facts_are_stored` / run_files `a_run_is_indexed_when_it_starts_and_when_it_finishes` / run `runs_are_listed_from_the_history_newest_first` |
| 書き込みが途中で止まる場合の整合性（ファイルと SQLite の両方） | 検証済み | history `a_terminal_record_that_fails_halfway_is_rolled_back_completely` / `the_schema_ties_the_outcome_to_the_finished_state` / `reconciling_never_takes_back_or_invents_a_result` / run_files `the_history_is_repaired_after_a_stop_at_every_point` / `a_damaged_or_foreign_terminal_record_is_not_a_result` |
| 件数が不明な結果を 0 として保存しない | 検証済み | history `unknown_counts_are_stored_as_null_not_as_zero` |
| `runs list` は終端レコードの無い run を finished と表示しない | 検証済み | cli 単体 `a_listed_run_follows_its_directory_not_only_the_index` / run `r15_…` / `gc_apply_is_refused_while_a_run_is_active`（実行中は running） |
| gc は所有を証明できるものだけを、保持期間を過ぎてから削除する | 検証済み | gc `only_runs_that_ended_before_the_retention_period_are_removed` / `an_interrupted_run_is_kept_until_it_is_old_as_well` / `what_testry_does_not_own_is_reported_and_left_alone` / `a_plan_is_kept_while_a_kept_run_refers_to_it` / `ownership_is_checked_again_right_before_removing` / run_files `only_real_run_directories_are_runs` / `a_start_record_of_another_run_or_plan_proves_nothing` / run `gc_removes_only_what_is_older_than_the_retention_and_owned` |
| `gc --dry-run` は何も変更しない・`gc --apply` は実行中の run があると拒否する | 検証済み | gc `a_workspace_without_stored_state_has_nothing_to_remove` / run `gc_in_a_workspace_without_runs_does_nothing` / `gc_apply_is_refused_while_a_run_is_active` |
| JSON Schema（plan / result / event / config / error / runs-list / interrupted-run / gc）と実際の出力の一致 | 検証済み | `crates/testry-cli/tests/schemas.rs`（5 件。実バイナリの出力を検証し、壊した文書が拒否されることも検査） |
| ベンチマーク（仕様 20 章） | 実施済み（範囲つき） | `make bench`。生データと環境情報は `docs/benchmarks/`、要約は `docs/benchmarks.md`。コンテナ環境の準備を含む比較は対象外（ADR-0006） |

注4: testry が所有する資源はプロセスと run ディレクトリだけなので（ADR-0006）、R15 の「所有資源の検出」は
「中断された run の検出」と「その run が残したプロセスの検出・回収」として検証している。
回収できるのは、testry が付けた目印を環境変数に持つプロセスだけである（`/proc` のある OS、環境を消していないプロセス）。

## テストの破壊検出の確認（ミューテーション）

通るだけのテストでないことを確かめるため、実装を一時的に壊して対応するテストが落ちることを確認した
（確認後は元に戻している）。M2 で確認したもの:

| 壊し方 | 落ちたテスト |
|---|---|
| trust の判定を常に許可にする | run `nothing_runs_until_the_configuration_is_trusted` / `a_job_definition_can_pin_the_trusted_configuration_digest` |
| Bun へ選択ファイルを渡さない（全ファイルが走る） | run `test_affected_runs_only_the_affected_test_file` ほか 2 件 |
| 取得済みの workspace ロックを無視する | run `r17_a_second_run_in_the_same_workspace_is_refused` |
| 実行直前の入力再確認を省く / cleanup の失敗を無視する / キャンセル後も投入する | exec の対応する P13 / R08 / R06 テスト |
| グループ外のプロセスを探さない | process `processes_that_left_the_group_…` ほか 2 件（契約テストの Playwright は、ランナー自身が片付けるため落ちない） |
| Playwright に `--forbid-only` を渡さない | contract `playwright_focused_tests_fail_the_suite` ほか 3 件 |
| retry 後の成功を passed と読む | contract `playwright_runs_the_whole_suite_and_keeps_statuses_apart` |
| Playwright に `--output` を渡さない | contract `playwright_runs_the_whole_suite_…`（作業ツリーに `test-results/` ができる） |
| Storybook の suite に `--project` を渡さない | contract `storybook_runs_the_stories_project_as_a_whole` ほか 1 件 |
| suite の command の検査を起動前に行わない | exec `a_command_its_adapter_refuses_…` / browser `a_playwright_command_that_narrows_the_suite_is_refused` |
| 開始記録の無いディレクトリを gc の削除対象にし、削除直前の所有確認も外す | gc `what_testry_does_not_own_is_reported_and_left_alone` / `ownership_is_checked_again_right_before_removing` |
| 終端レコードの無い run を interrupted にしない | history `reconciling_follows_the_run_directories` ほか 3 件 |
| `gc --apply` が workspace ロックを取らない | run `gc_apply_is_refused_while_a_run_is_active` |
