# 開発ガイド

## 前提環境

| 要件 | 備考 |
| --- | --- |
| Linux、または Windows + WSL2 | 主開発環境として想定 |
| Rust toolchain | 1.82 以降（`rustup`） |
| Docker Engine + Compose | M3 以降、疑似クラスタに必要 |

実 Slurm クラスタは開発環境では **ありません**。
日常の作業は unit test、in-process simulation、Docker 疑似クラスタに対して行います。

## ビルドと検査

```bash
cargo build
```

常に緑を維持すべき 3 つの検査（CI でも実行）:

```bash
cargo fmt --check
```

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

```bash
cargo test --all
```

## テスト階層

| Level | 対象 | 必要なもの |
| --- | --- | --- |
| 1 — unit / mock | domain model、config、parser、graph、state、rule、DB、protocol | なし |
| 2 — in-process simulation | 人工 observation を state → diagnosis → incident へ流す | なし |
| 3 — Docker 疑似クラスタ | 実 agent・実 controller・実 Slurm、障害注入 | Docker |
| 4 — VM / 実クラスタ | reboot、boot ID、NFS hard mount、D-state、実 GPU | VM または実機 |

Level 1 と 2 は、Rust toolchain さえあれば Docker 無しで必ず通ります。

## 疑似クラスタ

Docker Compose による 6 container の疑似クラスタが `dev/compose/` にあります。
controller 1、compute 3、fileserver 2 という構成です。

```bash
cd dev/compose
./scripts/up
```

`up` は container の起動だけでなく、controller API の応答・Slurm の node 登録・
storage service・agent 登録がすべて揃うまで待ってから戻ります。

```bash
./scripts/sentinel status          # クラスタ状態
./scenarios/stop-slurmd compute01  # 障害注入
./scripts/sentinel status          # 診断結果を確認
./scenarios/recover-all            # 復旧
./scripts/down                     # 停止
./scripts/reset                    # volume ごと作り直し
```

利用可能なシナリオ一覧と依存関係トポロジは
[`dev/compose/README.md`](../dev/compose/README.md) を参照してください。

Docker 統合テストは無印では実行されません（Docker が無い環境でも
unit / simulation テストが通るようにするため）。実行するには:

```bash
SENTINEL_DOCKER_TESTS=1 cargo test --test m3_pseudo_cluster -- --test-threads=1
```

## Docker で保証できないもの

疑似クラスタは service / process / network 障害を忠実に再現しますが、
以下は再現 **しません**。

* 実機 reboot の semantics と boot ID の変化
* kernel hard lock と真の D-state
* NFS hard-mount による kernel stall
* 実 systemd host の挙動
* SMART / NVMe、GPU、NIC のハードウェア障害
* IPMI / BMC、物理電源断

これらは level 4 の責務であり、コンテナで近似するのではなく
VM テスト要件として記録します。詳細は
[VM_VALIDATION.md](VM_VALIDATION.md) を参照してください。

## v1 受け入れテスト

`docs/IMPLEMENTATION.md` §97 の 18 段階を自動化してあります。

```bash
cd dev/compose
./scripts/acceptance
```

各段階で 1 つだけを既知の方法で壊し、Sentinel が
**それを** 指すこと、および **他のものを指さないこと** を検査します。
否定側の検査が同じくらい重要です —
停止した daemon に対して `HOST_UNREACHABLE` と言う監視は、
何も言わない監視より有害だからです。

## リポジトリ構成

```text
src/
  entity/       ManagedEntity、identity
  capability/   Capability とその解決
  dependency/   有向グラフ、cycle-safe traversal
  observation/  immutable な probe 結果
  probes/       Probe interface（配下に integration）
  state/        導出された health、debounce
  diagnosis/    typed rule
  incident/     correlation と lifecycle
  persistence/  SQLite repository
  config/       設定、優先順位、検証
  cli/          サブコマンド
migrations/     SQL migration（順に適用）
fixtures/       parser test 用の実コマンド出力
tests/          integration / simulation / scenario test
dev/compose/    Docker 疑似クラスタ（M3 以降）
```

## 規約

* core / library 層は typed error（`thiserror`）、CLI 境界は `anyhow` を使用します。
* `unwrap()` / `expect()` はテストと、失敗し得ないことが証明できる箇所に限ります。
* probe の失敗が daemon を panic させてはなりません。
* 外部コマンドは必ず timeout と出力サイズ制限のもとで実行します。
* `src/` に host 名・address・partition 名・storage topology を書きません。
  fixture・設定・テストが正しい置き場所です。

## 実装済みの probe

| Probe ID | 観測対象 | 必要 capability | 実行場所 |
| --- | --- | --- | --- |
| `host.metrics` | load / memory / pressure / uptime / boot ID | `host.metrics` | local |
| `network.tcp` | host への到達性 | `network.tcp` | local / remote |
| `ssh.service` | SSH banner | `ssh.server` | local / remote |
| `sentinel.agent` | agent の health endpoint | `sentinel.agent` | **remote のみ** |
| `systemd.unit` | systemd unit の状態 | `systemd` | local |
| `slurm.node` | scheduler から見た node 状態 | （Slurm discovery） | controller |
| `slurm.controller` | control plane の到達性 | （Slurm discovery） | controller |
| `nfs.client.mount` | mount 一覧・read-only 化 | `storage.nfs.client` | local |
| `nfs.client.io` | mount への実 I/O 応答性 | `storage.nfs.client` | local（**mount ごとに同時 1**） |
| `nfs.server.port` | export port の応答 | `storage.nfs.server` | local / remote |
| `nfs.server.exports` | export 一覧 | `storage.nfs.server` | local |
| `gpu.nvidia` | GPU 一覧・温度・メモリ | `gpu.nvidia` | local |
| `journal.events` | kernel / service event | `journal.read` | local（**同時 1**） |

probe の一覧・既定スケジュール・説明は `src/probes/catalog.rs` が唯一の出所です。
`sentinel config init` が書き出す設定ファイルも、`config check` が probe id の
妥当性を判定するのもここを見ています。probe を追加したら catalog に追加してください
（`every_probe_in_the_tree_is_listed` テストが強制します）。

運用者が `[probes]` で頻度を変更できます。override は probe の
`ProbeDefinition` 自体に適用されます。decorator で包まないのは、
いくつかの probe が自分の timeout を使って実行するコマンドを制限しているためで、
runner だけが知る timeout は「同じ名前の別の値」になってしまいます。

### journal probe

kernel event（OOM / I/O error / hung task / NVMe timeout / GPU Xid /
MCE / NFS server not responding / link down / thermal）を継続的に収集します。

**on-demand ではなく継続収集** である理由は、
ログが最も必要な瞬間は host が最もそれを渡せない瞬間だからです
（`SPEC.md` §1「reboot 後に原因情報が失われる」）。
運用者が見に行く時点では、証拠はすでに controller にあります。

query は 4 方向すべてで制限されています（`IMPLEMENTATION.md` §52）。

| 制限 | 値 |
| --- | --- |
| 時間 | 前回 scan 以降、最大 15 分 |
| priority | warning 以上 |
| 行数 | 500 |
| バイト数 | 1 MiB |

matched event は **事実として記録するだけ** で、診断結果とは等価に扱いません
（`SPEC.md` §83）。compute node での OOM kill は多くの場合、
job が想定どおりに動いた結果です。

container には systemd が無いため、
Docker 疑似クラスタでは `journal.read` が検出されず probe は動きません。
実 systemd 環境での検証は level 4 です。

### NFS probe の安全性

hang した NFS mount に触れた process は uninterruptible sleep に入り、
kill もできず、syscall も cancel できません。
30 秒ごとに全 mount へ `stat()` する agent は、障害中に
blocked thread を積み上げて死にます — 監視が最も必要な瞬間にです。

そのため probe を危険度で分けてあります。

* `/proc/self/mounts` を読むだけの probe は、障害中も安全に動作します。
* 実 I/O を行う probe は **mount ごとに同時 1 本** に制限され、
  前回が戻ってこない場合、次回は起動せずスキップします。
  thread は失われますが、失われるのは 1 本だけです。

`sentinel.agent` が remote のみなのは、
自分自身に「動いているか」を尋ねても Yes 以外を返し得ないためです。

## Probe の追加手順

1. Capability 名を決める（広く有用なら `src/capability/mod.rs::well_known` へ。
   型自体は任意の文字列を受け付けます）。
2. `src/probes/<integration>/` 配下に `Probe` を実装します。
3. payload には **raw fact のみ** を返します。結論を出してはいけません。
4. 新しい fact が根拠となる diagnosis rule があれば追加します。

state engine・incident engine・database schema への変更は不要なはずです。
もし必要になったなら、それは抽象化が漏れているということであり、議論に値します。
