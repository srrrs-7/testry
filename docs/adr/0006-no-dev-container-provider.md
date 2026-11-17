# ADR-0006: Dev Container provider を実装しない

- 状態: 採用（利用者の決定、2026-11-18）
- 対象: 仕様 22 章の M4（Colima・Dev Container の基準実装）、仕様 11 章の `DevContainerCliRuntime`

## 背景

仕様は実行環境として「Host」と「Colima 上の Docker + 公式 Dev Container CLI」の 2 つを定義し、
M4 で後者（使い捨ての Dev Container を testry が作成・所有・破棄する provider）を実装することになっていた。

一方、着手時の指示は「コンテナでの実行を基本として、ホスト側で runtime 実行等は行わない」であり、
公式 Dev Container CLI は Node.js 製である。両立させるには、Docker ソケットを渡したオーケストレーター用コンテナの中で
testry と CLI を動かす必要があり、強い権限を持つコンテナを増やす判断になる（ADR-0002 の未決事項 2）。

M3 の完了時にこの点を利用者へ確認し、次の回答を得た。

> devcontainer を使用せずに現在のコンテナ実行を前提とした構成を保持したまま進めて欲しい

## 決定

1. **testry は Dev Container を作らない。** Dev Container CLI を呼び出さず、Docker / Colima にも接続しない。
   M4 は実装しない。
2. **実行モデルは現在のまま**とする。testry はプロジェクトのコンテナの中で動き、テストはそのコンテナ内の
   子プロセスとして実行する（設定上の名前は `provider = "host"`。ADR-0002）。
   コンテナそのものの用意（イメージ、依存のインストール、ブラウザ）は、testry の外側（compose、CI のジョブ定義）の責務。
3. コンテナ外での `test` / `verify` は既定で拒否し、`TESTRY_ALLOW_EXECUTION_OUTSIDE_CONTAINER=1` を明示したときだけ
   実行する（ADR-0002 の未決事項 1 を、この内容で利用者が追認）。
4. 設定の契約は変えない。`provider = "devcontainer_cli"` の環境は設定として読み込めるが、
   `--environment` で指定する（または `default_environment` にする）と `unsupported_capability`（exit 2）で止まり、
   何も実行しない。別の provider で代用することはない。`doctor` はその旨を表示する。

## 影響

### 対象外になる受け入れ条件

Dev Container を所有・操作することを前提にした条件は、検証対象が存在しないため「対象外」とする
（「検証済み」とは書かない）。

| ID | 内容 | 扱い |
|---|---|---|
| R07 | コンテナ exec クライアントの停止時に専有コンテナも停止 | 対象外 |
| R09 | 利用者の編集用 Dev Container に触れない | 対象外（testry はどのコンテナにも触れない） |
| R10 | ラベル不一致の cleanup 拒否 | 対象外 |
| R11 | postCreate 完了前にテストを始めない | 対象外 |
| R12 | 未承認の initializeCommand を Host で実行しない | 対象外（`devcontainer.json` を読まず、lifecycle コマンドを実行しない） |
| R14 | context と DOCKER_HOST の矛盾 | 対象外（Docker へ接続しない） |

`host` provider に意味がある条件（R01〜R06、R08、R13、R15〜R18）は従来どおり扱う。
R15 の「所有資源の検出」は、testry が所有する資源がプロセスと run ディレクトリだけになるため、
「中断された run の検出」と「残ったプロセスが無いことの確認」に読み替える。

### 残る制約（この決定によって解消されないもの）

- **suite 間・run 間の隔離は無い。** すべてのテストが同じコンテナ・同じファイルシステム・同じネットワークで動く。
  資源の予約はスケジューリング上の予算で、OS による制限ではない。
- **環境の再現性は testry の外側に依存する。** どのイメージで、どの依存が入った状態で実行したかを testry は
  作り出さない。記録するのは OS / アーキテクチャ / 実際に確認したランナーの版だけである。
- 仕様 20 章の比較対象 4（Host と Colima の環境準備込み比較）は実施しない。

### コード

- `RuntimeProvider` ポートと `ProviderKind::DevcontainerCli`、lease の状態遷移の型は残す
  （設定契約とポートの形を保つため。実装は `HostRuntime` だけ）。
- 将来この決定を変える場合は、この ADR を置き換える新しい ADR を書き、ADR-0002 の未決事項 2
  （CLI をどこで動かすか）から検討し直す。
