# 運用ガイド

導入が済んだあと、**日々どう使うか**のガイドです。
障害が出たときの読み方と、よくある症状への対処が中心です。

> 導入がまだの場合は [GETTING_STARTED.md](GETTING_STARTED.md)、
> 本番構成へ広げる場合は [DEPLOYMENT.md](DEPLOYMENT.md) を先に。

## 前提

* production は systemd + native host + 単一 `sentinel` バイナリ。
* Sentinel は **監視対象を変更しません**。
  reboot・`systemctl restart`・remount・`scontrol update` を実行しません。
  診断と、read-only な調査コマンドの提示のみを行います。

## 導入

Rust ツールチェインは不要です。
[Releases](https://github.com/Lzh-Function/cluster-sentinel/releases) の
静的リンクバイナリ（x86_64 / aarch64）を配置します。

```bash
sha256sum -c sentinel-x86_64-unknown-linux-musl.sha256
sudo install -m 0755 sentinel-x86_64-unknown-linux-musl /usr/local/bin/sentinel

sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel
sudo sentinel install controller     # または agent
sudo chown -R sentinel:sentinel /etc/sentinel
```

**先に `/usr/local/bin` へ配置してから `install` を実行してください。**
ダウンロード先のまま実行すると、unit がそのパスを指してしまいます
（`install` は警告します）。

`install` は `/etc/sentinel` を作り、設定ファイル・systemd unit・
（controller のみ）cluster credential を生成して、残りの手順を表示します。
既存のファイルは上書きしません（`--force` を付けた場合のみ）。

`/var/lib/sentinel` は systemd が初回起動時に作ります。

生成された設定ファイルには全設定が既定値のまま書き出されています。
書き換えが必要なのは `CHANGE-ME` を含む行だけです。

```bash
sudo -u sentinel sentinel config check
sudo systemctl daemon-reload
sudo systemctl enable --now sentinel-controller
```

ノードが多い場合は [Ansible ロール](../deploy/ansible/) を使ってください。

手順の詳細は [DEPLOYMENT.md](DEPLOYMENT.md) を参照してください。

### 設定ファイルだけ生成する

```bash
sentinel config init --role agent --dry-run          # 中身を見る
sentinel config init --role agent --output ./a.toml  # 書き出す
```

## 監視頻度の変更

probe ごとに `[probes]` で変更します。書かれていない probe は既定のままです。

```toml
[probes."network.tcp"]
interval = "15s"
```

生成された設定ファイルに全 probe の既定値がコメントアウトされて入っているので、
変えたい行のコメントを外してください。
一覧は [CONFIGURATION.md](CONFIGURATION.md) の `[probes]` にあります。

## 段階的な導入

実クラスタは開発環境ではありません（`IMPLEMENTATION.md` §31）。
以下の順で導入してください。

| 段階 | 内容 | 確認すること |
| --- | --- | --- |
| 1 | controller のみ（read-only、Slurm discovery） | `sentinel status` が既存構成を正しく表示する |
| 2 | agent 1 台 | 登録される。`sentinel entity show <host>` が妥当 |
| 3 | agent 複数台 | 全台登録。`sentinel peers` で observer が付く |
| 4 | peer monitoring 有効化 | 誤検知が出ないことを数日観察 |
| 5 | notification 有効化 | まずテスト用の宛先へ |
| 6 | 全台展開 | |

**実クラスタで危険な障害注入を自動実行しないでください**
（`IMPLEMENTATION.md` §32）。
NFS server の停止、network-wide な iptables 変更、reboot、filesystem 操作は
管理者判断のもとでのみ行います。
Docker / VM で代替できるものはそちらで行ってください。

## 日常の確認

CLI は root か `sentinel` ユーザーで実行します。
設定ファイルは world-readable にしていません
（webhook URL 自体が credential を含みうるため）。

```bash
sudo -u sentinel sentinel status
```

```bash
sentinel status              # クラスタ全体。異常があれば exit 2
sentinel diagnose            # 何が壊れていて、なぜか
sentinel incident list       # 対応が必要な incident
sentinel incident show <id>  # 根拠・timeline・evidence
sentinel peers               # observer の割り当て
sentinel doctor              # この host から見た自分自身
sentinel prune --dry-run     # 保持期間を過ぎた記録の量
```

### この仕組みを他人に説明する

```bash
sentinel explain                 # 3 つすべて
sentinel explain capabilities    # 何が probe を有効にし、どう判定されるか
sentinel explain probes          # 各 probe が実際に何を実行するか
sentinel explain paths           # どの host を誰が、何で監視しているか
```

`status` は Sentinel が**何を結論したか**を言い、`explain` は
**それがどうやって分かるのか**を言います。

### 「全部 HEALTHY」を鵜呑みにしない

このツールでは、**正しく監視できている状態と、そもそも何も見ていない状態が
同じ見た目になります。** 全部 HEALTHY という表示だけでは、
どちらなのか区別できません。

実際に起きた例です。

- ある検査がどこからも実行されていなかった。全ノード HEALTHY のまま、
  数か月気づかれなかった
- open な CRITICAL の通知が一度も飛んでいなかった。`status` は
  「31 healthy」と表示していた

導入直後と、構成を変えたあとには、次の 2 つを見てください。

```bash
sentinel explain paths
```

各 host の **`watched by`** を見ます。**ここが空、または 1 台しかない host は、
実質的に監視されていません。** 到達性の判断には最低 2 つの独立した視点が
必要で、1 台では Sentinel は判断を保留します。

```bash
sentinel entity observations <host>
```

`WHEN` 列が現在時刻の近くで更新され続けているかを見ます。
特定の検査だけを追うこともできます。

```bash
sentinel entity observations <host> --probe nfs.server.exports
```

**「観測がありません」と出たら、その検査は本当に走っていません。**
capability が付いていない可能性が高いので、`sentinel entity show <host>` の
Capabilities を確認してください。

### 定期的に `sentinel audit` を回す

上の 2 つは手で確認する手順ですが、**確認し忘れれば同じことです。**
`sentinel audit` は「有効なのに観測を出していない probe」を挙げ、
あれば exit code 2 を返します。cron に置いてください。

```bash
sudo -u sentinel sentinel audit > /dev/null || echo "cluster-sentinel: 監視に穴があります"
```

沈黙している probe があると `status` の末尾にも 1 行出ます。

```
⚠ 1 probe(s) have never reported at all; run `sentinel audit` for which
```

**構成を変えた直後は必ず見てください。** capability の判定が変わって
probe が静かになるのは、変更した本人にも見えない形で起きます。

### 名前が重複する場合

storage entity は提供元ホストと**同じ名前**を名乗ります
（`host/filesrv02` と `storage/filesrv02`）。裸の名前で指すと
どちらか分からないため、その場合は候補が表示されます。

```bash
sentinel entity show host/filesrv02      # ホストそのもの
sentinel entity show storage/filesrv02   # そのホストが提供するストレージ
```

```
systemd
  meaning    systemd units can be inspected here
  detected   the directory /run/systemd/system exists
  enables    systemd.unit

systemd.unit
  runs       systemctl show <unit> --property=ActiveState,SubState,Result
  needs      systemd
  where      on the host itself, by its agent
  cadence    every 10s, timeout 5s
```

```
compute02
  reached at   192.0.2.22
  from itself  host.metrics, systemd.unit
  from others  network.tcp, sentinel.agent, ssh.service
  watched by   node03 (SameDomain), fs02 (Independent), node01 (Filler)
```

**`explain probes` の cadence は設定を反映した値**です。`[probes]` で
変更していれば、その値が出ます。停止している probe は `[DISABLED]` と表示されます。

### 結論ではなく、生の観測を見る

state や diagnosis が腑に落ちないときは、**何が観測されたのか**を直接見ます。

```bash
sudo -u sentinel sentinel entity observations <name>
sudo -u sentinel sentinel entity observations <name> --probe network.tcp --limit 100
```

```
WHEN                 PROBE               OBSERVER        STATUS        DETAIL
────────────────────────────────────────────────────────────────────────────
09-08 10:35:43       network.tcp         node02          ok            192.0.2.22:22 connected
09-08 10:35:42       network.tcp         head01          failed        no answer within 3s
```

**observer 列が肝です。** 「SSH は通るのに到達不能」のような一見矛盾した状態は、
たいてい観測者ごとに結果が違うだけで、この列を見れば矛盾ではなくなります。
`(itself)` はその host の agent 自身による観測です。

すべて `--json` に対応しているため、スクリプトから利用できます。

`sentinel status` は異常があれば exit code 2 を返します。

## 診断結果の読み方

| 診断 | 意味 | 最初に見るもの |
| --- | --- | --- |
| `HOST_UNREACHABLE` | 複数の独立した observer から到達不能 | console / BMC |
| `PATH_SPECIFIC_NETWORK_FAILURE` | 一部経路のみ異常。**host は生きている** | 両端の network |
| `SSH_SERVICE_FAILURE` | SSH のみ停止 | `systemctl status sshd` |
| `SENTINEL_AGENT_FAILURE` | 監視のみ停止。host は正常 | `systemctl status sentinel-agent` |
| `SLURMD_SERVICE_FAILURE` | host は正常、`slurmd` のみ | `journalctl -u slurmd` |
| `SLURM_ONLY_DEGRADATION` | host は完全に正常。Slurm 上のみ DRAIN | `scontrol show node <n>` の Reason |
| `SLURM_CONTROL_PLANE_FAILURE` | control plane 自体 | `scontrol ping` |
| `NFS_SERVICE_FAILURE` | fileserver は稼働、export service のみ | `exportfs -v` |
| `NFS_CLIENT_FAILURE` | 1 client のみ。**server は正常** | 当該 client の mount |
| `SHARED_STORAGE_FAILURE` | 同一 storage の複数 client | storage を提供する host |
| `RESOURCE_CONFIGURATION_MISMATCH` | 設定と実ハードウェアの不一致 | `slurm.conf` |

### Sentinel が言わないこと

**`POWER_OFF` は出力しません。** network の沈黙は電源状態の証拠ではありません。
電源断・NIC 故障・switch 障害は network から見れば同じです。
`HOST_UNREACHABLE` は「誰も到達できない」までしか主張せず、
その先は console / BMC を確認するよう促します。

**observer が 1 台の場合、到達性の診断を行いません。**
1 視点では host の死と経路障害を区別できないためです。
`sentinel peers` で observer が付いているか確認してください。

## Notification

```toml
[notification]
min_severity = "warning"

[[notification.webhooks]]
name = "ntfy"
url = "https://ntfy.example.org/cluster-sentinel"
```

送信されるのは **変化があったとき** だけです。

| 送信する | 送信しない |
| --- | --- |
| incident の発生 | 継続中の incident（何度 polling しても） |
| severity の上昇 | 変化のない状態 |
| 診断内容の変化 | `min_severity` 未満（ただし復旧は常に送る） |
| 復旧 / 解決 | maintenance 中の対象 |

重複排除は宛先ごとに行われるため、
Slack を止めても pager は止まりません。
controller と fallback notifier が同じ incident を検知しても、通知は 1 回です。

### 実機で通知経路を確かめる

宛先を設定したあと、**障害を待たずに**届くかどうかを確認できます。

```bash
sudo -u sentinel sentinel notify test
sudo -u sentinel sentinel notify test --provider ops        # 宛先を絞る
sudo -u sentinel sentinel notify test --severity critical   # 重大度を変えて経路を試す
```

```
ops                  sent
broken               FAILED: cannot reach http://... : error sending request

1 of 2 destination(s) failed.
```

**incident も database も重複排除も触りません。** 「この controller が、
叫ぶべき相手に届くか」だけを答えます。URL の打ち間違いを障害の最中に
知ることになるのが最悪なので、その前に答えられる必要があります。

送られる内容は、人が見てもフィルタが見ても**テストと分かる**ようにしてあります。
`fingerprint` は `sentinel-test-notification` 固定で、実際の incident と
衝突しません（衝突すれば本物の通知を黙らせてしまいます）。

`min_severity` の下でも送ります。floor は「起こす価値があるか」を決めるもので、
テストはそれには当たらないためです。確かめているのは**到達できるか**だけです。

> 宛先が受け取ったことと、人が気づくことは別です。
> 実際にメッセージが届いているかは受信側で確認してください。

### 全経路を確かめる

通知経路だけでなく、probe → 診断 → incident → 通知の全体を試すなら、
**最も影響の小さい実障害**を起こします。

```bash
ssh <node> sudo systemctl stop sentinel-agent
# 数十秒待つ -> SENTINEL_AGENT_FAILURE として通知されるはず
ssh <node> sudo systemctl start sentinel-agent
# 復旧通知（resolved: true）が届くはず
```

agent を止めてもジョブには影響しません。その間そのノードのローカル観測が
止まるだけです。**本番で試せる障害はこれが上限**だと考えてください。

## Maintenance window

計画作業中の通知を抑止します。

**抑止するのは通知だけです。**
observation・state・diagnosis は継続し、
異常な状態が healthy に書き換えられることはありません。
そうしなければ、作業中に発生した本物の障害が隠れ、
いつ始まったのかを後から再構成できなくなります。

incident が抑止されるのは、
**影響を受けている entity がすべて** maintenance 対象である場合のみです。
1 台の作業が、4 台を巻き込む障害を隠すことはありません。

## トラブルシューティング

### agent が登録されない

```bash
sentinel doctor                          # credential が設定されているか
systemctl status sentinel-agent
journalctl -u sentinel-agent -n 50
curl -fsS http://<controller>:7443/v1/health   # controller は生きているか
```

`Credential: NOT CONFIGURED` と出る場合、`/etc/sentinel/token` が
サービスユーザーから読めていません。

### 監視されていない host がある

capability が付いていない可能性があります。

```bash
sentinel entity show <host>   # Capabilities を確認
sentinel doctor               # その host で、各 capability の判定理由を表示
```

`sentinel doctor` は capability ごとに
「検出された / この host には無い / 設定で有効化 / role による推定」
のいずれかを表示します。

必要であれば設定で明示的に上書きできます。

```toml
[capabilities]
"storage.nfs.server" = "force"
```

### 到達性の診断が出ない

observer が不足しています。

```bash
sentinel peers    # observer の付いていない entity を表示する
```

`observer.peer` capability を持つ host を増やしてください。

### controller が停止した場合

agent は観測と spool を継続します。
controller 復旧後、spool は自動で再送されます（重複挿入は起きません）。

```bash
sentinel doctor   # spool の深さを確認
```

### agent が起動直後に落ち続ける

`systemctl status` が再起動ループだけを示し、理由が書かれていない場合、
**設定ファイルを `sentinel` ユーザーが読めていない**ことが多いです。
`install` は設定を mode 0640 で書くため、root 所有のままだと
サービスから開けません。

```bash
sudo chown sentinel:sentinel /etc/sentinel/*.toml && sudo systemctl restart sentinel-agent
```

（サービスユーザーが既にあるホストでは `install` が自動で所有者を
設定するようになっています。古いバージョンで作られたファイルだけ
手当てが必要です。）

### 通知が来ない

まず宛先そのものを試します。障害を待つ必要はありません。

```bash
sentinel notify test
```

**宛先ごとに 1 通だけ**送られます。届かない場合は URL か到達性の問題です。

届くのに障害通知が来ない場合、確認する順に:

1. `sentinel incident list` — そもそも incident になっているか。
   診断は出ていても、severity が `min_severity` を下回っていれば送られません
2. `[notification] min_severity` — 導入初期に `"critical"` にしていないか
3. maintenance window に入っていないか

通知は**状態が変わったときだけ**飛びます。続いている incident を
繰り返し通知することはありません。これは仕様です。

### 通知が一度に大量に来る

incident 1 件につき 1 通です。多数届いたのなら、多数の incident が
同時に開いたということです。webhook を初めて設定した直後は、
**それまでに開いていた incident がまとめて送られます**（一度だけ）。

送信間隔は既定で 1 秒空きます。**間引きではなく間隔をあける**ので、
通知が捨てられることはありません。webhook 側がより厳しいなら伸ばせます。

```toml
[notification]
min_interval = "3s"
```

### storage の健全性が UNKNOWN のまま

その storage を提供しているホストの **export 検査**が動いていません。

```bash
sentinel entity observations <fileserver> --probe nfs.server.exports
```

観測が無い場合、そのホストに `storage.nfs.server` capability が
付いていない可能性があります。

```bash
sentinel entity show host/<fileserver>
```

capability は `/etc/exports` の存在か `exportfs` の有無で判定されます。
ZFS の `sharenfs` で export している場合も、export の実体は
`/etc/exports.d/*.exports` にあり、そちらも読まれます。

### 身に覚えのないホストが「NFS が壊れている」と言われる

**export していないホストは、export ゼロでも障害になりません。**
障害として報告されるのは、**2049 番で何かが listen しているのに
export が空**の場合だけです（クライアントが接続できて拒否される状態）。

これに該当しないのに報告されるなら、その判定は v1.0 より前の挙動です。

## ディスク容量と retention

controller の database は書き込み一方です。何も消さなければ埋まります。
実測値は host 1 台あたり **1 日約 170 MB**（5 host のテストベッドで約 10 KB/s）。
100 ノードなら 1 日 17 GB、1 か月 500 GB です。

既定で prune は有効（`[retention]`、observation 14 日）なので、
host あたり約 2.4 GB で頭打ちになります。設定は
[`CONFIGURATION.md`](CONFIGURATION.md) の `[retention]` を参照してください。

prune は controller 内で 1 時間ごと、および起動時に実行されます。
手動でも実行できます。

```bash
sentinel prune --dry-run                  # 何が消えるかだけ見る
sentinel prune                            # 実行
sentinel prune --vacuum                   # 実行し、領域を filesystem に返す
sentinel prune --observations 3d --vacuum # この 1 回だけ保持期間を短くする
```

`--dry-run` の件数は実際の DELETE を実行して rollback したものです。
別の COUNT クエリではないため、本番と食い違うことはありません。

**prune はファイルサイズを縮めません。** 空きページは新しい記録に再利用されるため
増加は止まりますが、領域を OS に返すには `--vacuum` が必要です
（database 全体を書き直すため自動では実行しません）。

数か月分が溜まった database に対する最初の 1 回は時間がかかり、
その間 write lock を保持します。agent は spool で耐えますが、
`sentinel prune` を使って任意のタイミングで実施することを推奨します。

**削除されないもの:** open な incident（年齢に関わらず）、
生存している incident / diagnosis が参照している observation、
各 entity の直近 `keep_per_entity` 件。

## バックアップ

```bash
systemctl stop sentinel-controller
cp /var/lib/sentinel/sentinel.db /path/to/backup/
systemctl start sentinel-controller
```

WAL mode のため、稼働中のコピーは `sqlite3 .backup` を使用してください。

```bash
sqlite3 /var/lib/sentinel/sentinel.db ".backup /path/to/backup/sentinel.db"
```

## アップグレード

**controller を先に、agent を後に。** binary version と protocol version は
分離されているので、同一 protocol version であれば混在状態でも動きます。

新しいバイナリを置いて再起動する、それだけです。設定・database・credential
はそのまま残ります。database の migration は controller の起動時に自動で
適用されます。

### 1. controller

```bash
ARCH=$(uname -m)
curl -fsSLO "https://github.com/mizuno-group/cluster-sentinel/releases/latest/download/sentinel-${ARCH}-unknown-linux-musl"
curl -fsSL "https://github.com/mizuno-group/cluster-sentinel/releases/latest/download/sentinel-${ARCH}-unknown-linux-musl.sha256" | sha256sum -c
```

置き換える前に、**今の設定が新しいバイナリで通ることを確認**します。
ここで落ちるなら、置き換えてから気づくより先に分かります。

```bash
chmod +x sentinel-*-unknown-linux-musl && sudo -u sentinel ./sentinel-*-unknown-linux-musl --config /etc/sentinel/config.toml config check
```

```bash
sudo mv sentinel-*-unknown-linux-musl /usr/local/bin/sentinel && sudo systemctl restart sentinel-controller
```

> **`cp` や `install` ではなく `mv` を使ってください。**
> どちらも既存ファイルを truncate しようとするため、
> **実行中のバイナリに対しては `Text file busy` で失敗します。**
> `mv` は rename なので、動いているプロセスは古い実体を掴んだまま無事です。

```bash
sentinel version && systemctl status sentinel-controller --no-pager
```

**controller と agent が同居しているホスト**（[DEPLOYMENT.md §9.10](DEPLOYMENT.md#910-controller-に-agent-を同居させる)）
では、バイナリは 1 つなので両方を再起動します。

```bash
sudo systemctl restart sentinel-controller sentinel-agent
```

### 2. agent（各ノード）

controller が動いていることを確認してから、同じ手順を各ノードで行います。
ノードが多い場合は [Ansible ロール](../deploy/ansible/)
の `sentinel_version` を変えて再実行してください。

ロールの `sentinel_version` は**リリースごとに更新されている**ので、
まず `git pull` してから流すのが確実です。指定が古いままだと
「もう入っている」と判断されて `changed=0` で終わります。

```bash
git -C <このリポジトリ> pull && ansible-playbook -i inventory.ini site.yml -K
```

一度きり別のバージョンにしたい場合は、`v` を付けて指定します
（**タグ名がそのまま URL に入る**ので、`v` を落とすと 404 になります）。

```bash
ansible-playbook -i inventory.ini site.yml -K -e sentinel_version=v1.0.0
```

agent が止まっている間の観測は spool に溜まり、復帰後に送られます。

### systemd unit が更新された場合

リリースノートに unit の変更が書かれている場合のみ必要です。

```bash
sudo sentinel install controller --force    # または agent
sudo systemctl daemon-reload
sudo systemctl restart sentinel-controller
```

**`--force` は設定ファイルと unit を上書きします。** 設定を手で調整している
場合は、先に控えを取ってください。

```bash
sudo cp /etc/sentinel/config.toml /etc/sentinel/config.toml.bak
```

**credential は `--force` でも上書きされません。** 入れ替わると全 agent が
一斉に締め出されるため、「ファイルを書き直す」という意味の flag が
巻き込んでよい対象ではないからです。意図的に更新する場合は、
ファイルを削除してから `install` を実行し、**全 host に配り直してください。**

### 切り戻し

前のバイナリに戻して再起動するだけです。database schema は後方互換であり、
新しいバージョンが適用した migration が古いバイナリを壊すことはありません。

```bash
sudo install -m 0755 /path/to/previous/sentinel /usr/local/bin/sentinel
sudo systemctl restart sentinel-controller
```

## v1 で行わないこと

* 自動復旧（reboot / restart / remount / `scontrol update`）
* LLM による診断
* controller の HA
* Prometheus / Grafana の置き換え
* per-node credential（cluster 共有 token + mutual TLS までが v1）
* 遠隔からの読み取り API（参照 CLI は controller 上で実行する必要があります）

いずれも core architecture を変更せずに追加できる設計にしてありますが、
v1 の範囲外です。
