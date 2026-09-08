# VM / 実クラスタ検証要件 (Level 4)

Docker 疑似クラスタでは再現できず、VM または実機での検証が必要な項目の一覧です
（`docs/IMPLEMENTATION.md` §28-§32, M11）。

**再現できないものを「再現できたことにしない」** ための文書です。
container での検証が real machine の検証を代替すると仮定した箇所は、
実際に壊れたときに最も高くつきます。

## 検証環境

最小構成で十分です。

```text
ctrl01      controller + slurmctld
compute01   agent + slurmd
filesrv01   agent + 実 NFS server
```

VM を日常の主開発環境にする必要はありません。
以下の項目を検証する必要が生じた段階で用意してください。

## 検証項目

### 1. Reboot と boot ID

| 項目 | 手順 | 期待される結果 |
| --- | --- | --- |
| boot ID の変化検出 | `compute01` を reboot | `host.boot` observation が記録され、`previous_boot_id` と `boot_id` が異なる |
| agent 再起動との区別 | `systemctl restart sentinel-agent` | reboot として記録され **ない** |
| reboot 後の復帰 | reboot 完了まで待つ | `ReturnToService=1` により Slurm へ自動復帰し、その遷移が記録される |

Docker で再現できない理由: container に boot semantics が無く、
`/proc/sys/kernel/random/boot_id` は host のものを見ています。

### 2. NFS hard mount と D-state

**最も重要な検証項目です。** Sentinel の NFS probe 設計全体が、
この挙動を前提にしています。

| 項目 | 手順 | 期待される結果 |
| --- | --- | --- |
| hard mount の停止 | `filesrv01` の NFS service を停止（client は hard mount） | `nfs.client.io` が `NFS_TIMEOUT` または `NFS_STUCK` を報告 |
| **thread が蓄積しないこと** | 停止状態を 10 分以上維持 | agent のスレッド数が増え続けない。`max_outstanding = 1` が効いている |
| agent の生存 | 同上 | agent が応答を続け、`nfs.client.mount` など他の probe は動作を続ける |
| D-state の観測 | `ps -eo stat,comm \| grep '^D'` | blocked task は **1 本のみ** |
| 復旧 | NFS service を再開 | probe が復帰し、state が recovery する |

Docker で再現できない理由: container 内の hang が host を巻き込み得るため、
`storage-service` は userspace の停止を模擬しているだけです。
`degrade-storage hang` は D-state ではありません。

### 3. 実 systemd

疑似クラスタは cgroup v2 を実階層で使用していますが、
**scope を作成するのは systemd ではなく slurmd 自身です**
（`IgnoreSystemd=yes`）。container に systemd も dbus も無いためです。
production の経路は未検証であり、ここで確認します。

| 項目 | 手順 | 期待される結果 |
| --- | --- | --- |
| systemd 管理下の cgroup scope | 通常どおり `slurmd` を起動 | dbus 経由で scope が作成され、cgroup エラーが出ない |
| unit 状態の取得 | `systemctl stop slurmd` | `systemd.unit` probe が `ActiveState=inactive` を報告 |
| failed unit | unit を意図的に失敗させる | `Result=exit-code` を検出 |
| hardening 下での動作 | 生成された unit で起動 | `ProtectSystem=strict` 下で全 probe が動作する |
| 権限不足の扱い | 非特権ユーザーで SMART 等を試行 | `UNSUPPORTED` であり `FAILED` ではない |

Docker で再現できない理由: container では supervisord を使用しています。

### 4. 実 GPU

| 項目 | 期待される結果 |
| --- | --- |
| `nvidia-smi` の実出力 | parser が全 field を正しく取得する |
| GRES との突合 | `slurm.conf` の GPU 数と一致すれば診断なし |
| GPU 数の不一致 | `GPU_CONFIGURATION_MISMATCH` |
| driver 異常時 | `nvidia-smi` が失敗しても agent が生存する |

### 5. ハードウェア障害

| 項目 | 備考 |
| --- | --- |
| SMART / NVMe の異常 | v1 では probe 未実装。将来 capability 追加で対応 |
| NIC のハードウェア故障 | `HOST_UNREACHABLE` になるが `POWER_OFF` とは言わないこと |
| 物理電源断 | 同上。BMC 無しでは電源状態を主張しないこと |

### 6. Kernel event

container には systemd が無いため、疑似クラスタでは `journal.read` が
検出されず、`journal.events` probe は動作しません。**実機での検証が必須です。**

| 項目 | 手順 | 期待される結果 |
| --- | --- | --- |
| journal の読み取り | `sentinel doctor` | `journal.read` が detected |
| event の収集 | `logger -p kern.err "test I/O error on dev sda1"` 等 | observation の evidence に記録される |
| OOM | 実際に発生させる、または過去ログで確認 | `oom` として記録されるが、それ自体で host を degraded にしない |
| I/O error / hung task | 同上 | `serious` として記録され、host component が degraded になる |
| **reboot をまたいだ保存** | event 発生後に reboot | controller 側に event が残っており、reboot 後も参照できる |
| 出力上限 | 大量に log が出る状態で確認 | truncate され、その事実が記録される |

### 7. Clock skew

| 項目 | 手順 | 期待される結果 |
| --- | --- | --- |
| skew の検出 | 1 台の時刻を意図的にずらす | heartbeat が `clock_skew_ms` を報告する |
| observation を捨てないこと | 同上 | skew があっても observation は保存される（`IMPLEMENTATION.md` §47）|

## 実クラスタでの障害注入について

`mizuno_cluster` は開発環境ではありません（`IMPLEMENTATION.md` §31, §32）。

**自動実行してはならないもの:**

* NFS server の停止
* network 全体に対する iptables 変更
* reboot
* filesystem 操作

これらは管理者の判断のもとでのみ、計画して実施してください。
Docker / VM で代替できるものはそちらで行ってください。

導入順は [OPERATIONS.md](OPERATIONS.md) の「段階的な導入」に従ってください。

## 現在の検証状況

| Level | 内容 | 状況 |
| --- | --- | --- |
| 1 | unit / mock | 自動化済み・CI 実行可能 |
| 2 | in-process simulation | 自動化済み・CI 実行可能 |
| 3 | Docker 疑似クラスタ | 自動化済み（`dev/compose/scripts/acceptance`、23 項目）。cgroup は v2 実階層だが scope 作成は systemd 経由ではない |
| 4 | VM / 実機 | **未実施**。本書が要件一覧 |

## TLS

疑似クラスタは平文 HTTP のまま動作します（TLS は追加的な設定であり、
既定の経路を変えないことを確認するため）。

protocol 部分は `tests/tls.rs` が実際の TLS listener と実際の client、
その場で生成した証明書で検証しています（mutual TLS の受理・拒否を含む）。

daemon の設定読み込み経路は、本 repository の開発環境で
`sentinel controller` を mutual TLS 設定で起動し、
`curl` で外部から確認済みです（平文接続・CA 無し・client 証明書無しの
3 通りがすべて拒否され、client 証明書ありのみ成功）。

実機で確認すべきこと:

| 項目 | 手順 | 期待される結果 |
| --- | --- | --- |
| 起動 | controller の log | `controller listening ... tls=true` |
| 証明書チェーン | `openssl s_client -connect <host>:7443 -CAfile <ca>` | `Verify return code: 0` |
| agent の接続 | agent の log | 登録が成功し、`insecure_skip_verify` の警告が出ないこと |
| 証明書の期限切れ | 期限切れ証明書で起動 | agent が接続を拒否し、log に理由が出ること |
| 材料の欠損 | `key` を読めない状態にする | controller が**起動に失敗する**（平文で起動しないこと） |

## Retention

`sentinel prune` は疑似クラスタの実データに対して確認済みです
（10,350 observation を削除、`--vacuum` で 10.8 MiB → 2.3 MiB、
削除後も診断は正常）。

実機で確認すべきこと:

| 項目 | 手順 | 期待される結果 |
| --- | --- | --- |
| 数か月分に対する初回 prune | 実際に実行し、所要時間を測る | 完了すること。その間 agent が spool で耐えること |
| 定期実行 | controller を 1 時間以上動かす | log に prune の記録が出ること |
| 容量の頭打ち | 数週間の運用後に database サイズを確認 | 保持期間に対応する値で安定すること |
| 長期停止からの復帰 | controller を数日停止して起動 | 起動時の prune が滞留分を処理すること |
