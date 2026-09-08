# アーキテクチャ

本書はコードの構成と、その構成が守ろうとしている不変条件を説明します。
**何を** 作るかについては `docs/SPEC.md` が source of truth であり、
本書は **どう** 作るか、および読者が違う実装を選びそうな箇所についての理由を記録します。

個別の重大な設計判断は [`docs/adr/`](adr/) に ADR として記録します。

## 一文での説明

Sentinel はインフラを capability と dependency を持つ entity の集合としてモデル化し、
複数地点から観測し、**単一の観測結果ではなく観測の「パターン」から** 原因を導出します。

## パイプライン

```text
Probe ──▶ Observation ──▶ State ──▶ Diagnosis ──▶ Incident
```

各矢印は厳格な境界であり、各段階は異なる問いに答えます。

| 段階 | 答える問い | 禁止事項 |
| --- | --- | --- |
| Probe | 今この瞬間、何が測定できるか | 解釈。probe が `HOST_UNREACHABLE` を出してはならない |
| Observation | 何が、誰から、いつ観測されたか | 変更。observation は append-only |
| State | この component は健全か | 理由の説明 |
| Diagnosis | 原因は何か | 誰を起こすかの決定 |
| Incident | 運用者が対処すべきことは何か | 証拠の捏造 |

この分離の理由は **反証可能性** です。Sentinel が「fileserver が原因だ」と述べたとき、
運用者は diagnosis から、それを導いた rule へ、その rule が使った observation へ、
さらにその timestamp と observer へと遡れなければなりません。
probe が結論を返す設計では、この鎖が最初の一歩で切れてしまいます。

## モジュール依存方向

```text
integrations (probes/, inventory/)
        │
        ▼
   observation ──▶ state ──▶ diagnosis ──▶ incident
```

Core は integration へ依存しません。具体的には、core module が
`use crate::probes::slurm::...` と書いてはなりません。
Slurm・NFS・NVIDIA が core へ到達する経路は以下のみです。

* capability 文字列
* JSON payload を持つ observation
* dependency edge

これが SPEC.md §146（NFS から CephFS への置換）を、書き直しではなく
設定変更にしている理由です。

## リポジトリ構成

```text
src/
  entity/         ManagedEntity と identity
  capability/     Capability とその解決
  dependency/     有向グラフと cycle-safe traversal
  observation/    immutable な probe 結果
  state/          導出された health、debounce、state engine
  diagnosis/      rule engine と rule 群
  incident/       correlation と lifecycle
  ─────────────── 以上が core。integration へ依存しない
  probes/         Probe interface と実装
  inventory/      InventoryProvider と merge
  integrations/   技術固有の知識（Slurm 等）
  agent/          agent daemon、local/peer probe、spool、RPC
  controller/     controller、API、observer、peer assignment
  protocol/       wire protocol
  command/        外部コマンド実行（timeout / allowlist）
  persistence/    SQLite repository
  notification/   通知、重複排除、maintenance
  config/         設定、優先順位、検証
  cli/            サブコマンド
```

`SPEC.md` §183 の推奨構成からの意図的な差異が 1 点あります。

**`integrations/` を追加してあります。**
推奨構成は `inventory/slurm/` と `probes/slurm/` の双方を挙げていますが、
`scontrol` の出力をどう解釈するかという知識は 1 箇所にあるべきです。
`integrations/slurm/` がその知識を持ち、
`inventory/` と `probes/` は core 向けの trait と薄い adapter を持ちます。

この配置は依存方向を変えません。core は依然として integration へ依存しません。

## Identity

Entity の natural key は `(environment, entity_type, canonical_name)` です。
内部 ID はその key から導出される UUIDv5 です
（[ADR 0001](adr/0001-deterministic-entity-identity.md) 参照）。

**IP address は identity ではありません。**
1 host は management / storage / interconnect network にまたがる複数 address を持ち、
それらは host の同一性とは無関係に変化します。
address は `entity_addresses` に 1 entity 複数行として保持します。

同様に Slurm `NodeName` は hostname ではありません。
両者は「同一である」という仮定ではなく、保存された mapping で関連付けます。

## Role ではなく Capability

Probe は capability によって有効化されます。
Role は grouping と default 提示のための運用者向けラベルであり、
単独で probe を起動することはありません。

これが防ぐ障害は現実的なものです — 組織変更で付け替えられたラベルによって
host が静かに監視対象から外れ、誰も見ていないものが壊れるまで誰も気付かない、
という事態です。
解決順序は `src/capability/resolve.rs` にあり、各優先規則にテストがあります。

## Dependency は汎用有向グラフ

Tree でも DAG でもありません。
運用上の依存関係には cycle が実在します — controller が storage に依存し、
その storage を export する host が controller の scheduler に依存する、といった形です。
Sentinel は運用者に不正確な記述を強いる代わりに、これをそのまま受け入れます。

`src/dependency/graph.rs` のすべての traversal は visited set を持ち、
cycle と self-loop に対するテストがあります。

Group は **宣言せず導出** します。「あの fileserver の背後にある node 群」は
`group_by_shared_upstream` の結果であり、storage domain の追加にコード変更は不要です。

## Sentinel が結論しないこと

2 つの抑制が設計上重要であり、いずれも慣習ではなく diagnosis rule として強制されます。

* **単一 observer の失敗は host 障害ではない。**
  ある observer だけが失敗し他が成功しているなら、それは *path* の障害です。
  peer monitoring が存在する理由がこれです。
* **network の沈黙は電源状態ではない。**
  network 証拠から導ける最強の結論は `HOST_UNREACHABLE` までです。
  `POWER_OFF` には BMC / PDU 等の out-of-band 証拠が必要であり、
  v1 はそれを収集しないため、v1 は決してそう述べません。

## Probe 層の安全制約

| 制約 | 理由 |
| --- | --- |
| 外部コマンドは必ず timeout を持つ | hang した `scontrol` が scheduler を巻き込んで停止させてはならない |
| 出力はサイズ制限し、truncate した事実を記録する | `journalctl` は GB 単位を返し得る |
| 1 mount あたり同時 filesystem probe は最大 1 | blocking した NFS syscall は uninterruptible sleep にあり、2 本目は助けにならず kill もできない |
| probe の panic が daemon を落とさない | 障害時に死ぬ監視は、監視が無いより悪い |
| RPC は remote command 実行を提供しない | agent は事前コンパイル済み probe のみを実行する |

## v1 では自動復旧を行わない

Sentinel は reboot・restart・remount・`scontrol update` を実行しません。
診断を行い、運用者が実行し得る **read-only** の調査コマンドを提示するのみです。

行動を起こせるほど確信のある診断エンジンとは、一度も誤ったことのないエンジンです。
本エンジンはまだその資格を得ていません。
recommended action は diagnosis 上のデータとして保持しているため、
将来「運用者による明示的な opt-in のもとで実行する」機能を、
設計変更なしに追加できます。

## Core diagnosis に LLM を使わない

Core diagnosis は決定的な typed Rust です。
同じ observation は常に同じ diagnosis を生み、発火した rule は ID で記録されます。
将来、言語モデルが incident を人間向けに **要約** することはあり得ますが、
何が壊れているかを **決定** することはありません。

## 永続化

WAL mode の SQLite を、`src/persistence/` の repository 型経由で使用します。
migration はバイナリへ埋め込まれており、単一ファイルが自身の DB を構築できます。

特記すべき schema 上の判断:

* `controllers` は `environment` に unique 制約を持ちません。
  v1 は controller 1 台構成ですが、2 台目を **禁止する** schema は
  HA を「機能追加」ではなく「migration」にしてしまいます。
* `observations` は append-only です。schema 上、行を UPDATE する箇所はありません。
* `observations.id` は生成元 agent が採番します。これが spool 再送を idempotent にします。

## Deployment data は data のままにする

`src/` 配下に host 名・address・partition 名・storage topology は一切現れません。
これらの値は `fixtures/`・設定ファイル・`dev/compose/`・テストにおいては正当です。
`src/` 配下を production host 名で grep してヒットしたらそれはバグであり、
`tests/` がこれを強制します。
