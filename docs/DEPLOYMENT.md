# 実クラスタ導入マニュアル

実際の計算クラスタへ Cluster Sentinel を導入する手順書です。

前提として、**Sentinel は監視対象を一切変更しません。**
reboot・`systemctl restart`・mount 操作・`scontrol update` を実行しません。
したがって導入自体がクラスタの動作を変えることはありませんが、
段階的に入れることを強く推奨します（[§7](#7-段階的導入)）。

日常運用は [OPERATIONS.md](OPERATIONS.md)、
設定項目の網羅的な説明は [CONFIGURATION.md](CONFIGURATION.md) を参照してください。

---

## 目次

1. [事前確認](#1-事前確認)
2. [構成の決定](#2-構成の決定)
3. [バイナリの配置](#3-バイナリの配置)
4. [Controller の構築](#4-controller-の構築)
5. [cluster credential の配布](#5-cluster-credential-の配布)
6. [Agent の展開](#6-agent-の展開)
6.5 [監視頻度を変える](#65-監視頻度を変える)
7. [段階的導入](#7-段階的導入)
8. [SSH ポートが 22 でない場合](#8-ssh-ポートが-22-でない場合)
9. [その他の非標準構成](#9-その他の非標準構成)
10. [導入後の確認](#10-導入後の確認)
11. [設定ファイルテンプレート](#11-設定ファイルテンプレート)
12. [チェックリスト](#12-チェックリスト)

---

## 1. 事前確認

### 必要なもの

| 項目 | 内容 |
| --- | --- |
| OS | Linux（systemd 前提） |
| 権限 | 各 host の root（インストール時のみ。実行は非特権ユーザー） |
| network | agent → controller への TCP 到達性（既定 7443） |
| | controller / peer → 各 host への TCP 到達性（SSH ポート、agent ポート 7444） |

Sentinel は Slurm を **変更しません**。既存の `slurm.conf` を書き換える必要はありません。

### 確認しておく情報

導入前に以下を控えてください。設定に必要です。

```bash
# controller になる host 名と、agent から到達可能なアドレス
hostname -f

# Slurm の controller と node 定義（あれば）
grep -E "^(SlurmctldHost|ControlMachine|NodeName)" /etc/slurm/slurm.conf

# SSH のポート（22 以外なら §8 を参照）
grep -iE "^\s*(Port|ListenAddress)" /etc/ssh/sshd_config

# Slurm の外にある host（fileserver 等）の一覧と、その storage 構成
```

---

## 2. 構成の決定

以下を決めます。

| 決めること | 例 | 備考 |
| --- | --- | --- |
| environment 名 | `mizuno-lab` | 全 host で一致させる |
| controller を置く host | `parent` | source code には現れない。設定だけの問題 |
| scheduler entity 名 | `mizuno_cluster` | Slurm の ClusterName に合わせると分かりやすい |
| observer にする host | controller / fileserver / 一部 compute | **3 台以上**を推奨（後述） |
| storage の依存関係 | どの node がどの fileserver を使うか | 誤診断を避けるために重要 |

### observer を 3 台以上にする理由

observer が 1 台しかない場合、Sentinel は **到達性の診断を行いません。**
1 視点では「host が死んだ」と「経路が切れた」を区別できないためです。

異なる障害ドメインの host を選んでください。
同じ storage の背後にいる 3 台は、視点 1 つを 3 回数えているだけです。

### storage の依存関係を書く理由

これを書かないと、複数 node の storage 障害が
`SHARED_STORAGE_FAILURE`（原因は fileserver）ではなく、
個別の `NFS_CLIENT_FAILURE` として報告されます。
5 人が 5 つの症状を追いかけることになります。

---

## 3. バイナリの配置

```bash
cargo build --release        # または配布された成果物を使用
sudo install -m 0755 target/release/sentinel /usr/local/bin/sentinel
sentinel version
```

同一アーキテクチャであれば全 host に同じバイナリを配布できます。
host ごとのビルドは不要です。

---

## 4. Controller の構築

### 4.1 install

```bash
sudo sentinel install controller
```

これ 1 回で以下が生成されます。

| 生成物 | 内容 |
| --- | --- |
| `/etc/sentinel/config.toml` | **全設定を既定値のまま書き出した設定ファイル**（説明つき） |
| `/etc/systemd/system/sentinel-controller.service` | hardening 済み systemd unit |
| `/etc/sentinel/token` | cluster credential（32 byte 乱数、mode 0400） |

**既存のファイルは上書きしません。** バージョンアップ後にもう一度実行しても、
調整済みの設定や credential はそのまま残ります
（credential が入れ替わると全 agent が一斉に締め出されるため）。
上書きしたい場合のみ `--force` を付けてください。

内容を先に確認したい場合:

```bash
sentinel install controller --dry-run
```

credential を secret manager などで別管理している場合:

```bash
sudo sentinel install controller --no-credential
```

### 4.2 サービスユーザー

`install` が実行後に案内しますが、以下は手で行う必要があります。

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel
sudo install -d -o sentinel -g sentinel -m 0750 /var/lib/sentinel
sudo chown -R sentinel:sentinel /etc/sentinel
```

### 4.3 設定の仕上げ

生成された `/etc/sentinel/config.toml` のうち、
**書き換えが必要なのは `CHANGE-ME` を含む行だけ**です。
controller の場合は `environment` の 1 行です。

それ以外はすべて既定値がそのまま書き出されており、
変更したい行のコメントを外すか値を書き換えます。
Slurm の外にある fileserver や依存関係は、
ファイル内のコメント例を参考にしてください（§11 にも同じものがあります）。

### 4.4 検証

**起動前に必ず実行してください。**

```bash
sudo -u sentinel sentinel config check
```

検出できる問題をすべて報告します（最初の 1 件で止まりません）。
`error` が 1 つでもあれば起動しません。

`warning` は許容されます。特に
「宣言されていない entity への依存」は、
Slurm discovery や agent registration から到着する予定のものであれば正常です。

### 4.5 起動

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now sentinel-controller
systemctl status sentinel-controller
journalctl -u sentinel-controller -f
```

### 4.6 動作確認

```bash
curl -fsS http://localhost:7443/v1/health
sentinel status
sentinel dependency list
```

この時点では agent がいないため、多くの entity が `UNKNOWN` です。
**これは正常です。** 観測していないものを healthy とは呼びません。

---

## 5. cluster credential の配布

**environment 内の全 host で同一の値**を使用します。
controller の `install` が生成したものを、各 host に配布してください。

```bash
sudo scp /etc/sentinel/token <host>:/etc/sentinel/token
# 配布先で
sudo chown sentinel:sentinel /etc/sentinel/token
sudo chmod 0400 /etc/sentinel/token
```

> credential 無しでは controller も agent も **起動を拒否します**。
> 未認証で動作するモードはありません。

配布に scp を使う場合、経由地にファイルを残さないよう注意してください。

`sentinel install agent` は credential を生成しません。
agent が自前で生成すれば、クラスタの誰も知らない credential ができてしまい、
「設定の問題」が「認証の失敗」として現れることになるためです。

---

## 6. Agent の展開

```bash
sudo sentinel install agent
```

生成物は設定ファイルと systemd unit です。
書き換えが必要なのは `CHANGE-ME` を含む 2 行だけです。

```toml
environment = "CHANGE-ME-environment"                 # controller と一致させる
controller_address = "CHANGE-ME-controller-host:7443" # controller のアドレス
```

capability は agent が自動検出するため、列挙する必要はありません。

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel
sudo install -d -o sentinel -g sentinel -m 0750 /var/lib/sentinel
sudo chown -R sentinel:sentinel /etc/sentinel
# credential を配置（§5）
sudo -u sentinel sentinel config check
sudo systemctl daemon-reload
sudo systemctl enable --now sentinel-agent
```

### 確認

```bash
# この host が Sentinel からどう見えるか、capability の判定理由つき
sentinel doctor

# controller 側から
sentinel entity show <hostname>
```

`sentinel doctor` は capability ごとに
「detected on this host」「not present on this host」
「forced on by configuration」「suggested by a role」
のいずれかを表示します。想定と違う場合はここで分かります。

---

## 6.5 監視頻度を変える

生成された設定ファイルには全 probe の既定値が
コメントアウトされた状態で書き出されています。
変えたい行のコメントを外してください。

```toml
# 大規模クラスタで負荷を下げる
[probes."network.tcp"]
interval = "15s"

# この環境では GPU を別系統で見ている
[probes."gpu.nvidia"]
enabled = false
```

書かれていない probe は既定のまま動きます。
存在しない probe id を書くと `config check` が error にします
（黙って無視されると「変更したつもりで変わっていない」状態になるため）。

`max_outstanding` は引き下げしかできません。
`nfs.client.io` と `journal.events` は同時実行 1 に固定されており、
blocking syscall を積み上げないための制約は設定で覆せません。

一覧は [CONFIGURATION.md](CONFIGURATION.md) の `[probes]` にあります。

---

## 7. 段階的導入

実クラスタは開発環境ではありません。以下の順で進めてください。

| 段階 | 作業 | 確認すること | 目安 |
| --- | --- | --- | --- |
| 1 | controller のみ | `sentinel status` が既存構成を正しく表示 | 1 日 |
| 2 | agent 1 台（重要度の低い node） | 登録される。`sentinel entity show` が妥当 | 1 日 |
| 3 | agent 数台 | 全台登録。誤検知が出ない | 2-3 日 |
| 4 | `observer.peer` を付与 | `sentinel peers` で observer が 3 台付く | 2-3 日 |
| 5 | storage の依存関係を記述 | `sentinel dependency list` が実構成と一致 | |
| 6 | notification を有効化 | **まずテスト用の宛先へ** | 1 週間 |
| 7 | 全台展開 | | |

各段階で数日おき、**誤検知が出ないこと**を確認してから次へ進んでください。
誤検知に慣れた運用者は、本物の警告も無視するようになります。

### 実クラスタでの障害注入について

**自動実行してはなりません。**

* NFS server の停止
* network 全体への iptables 変更
* reboot
* filesystem 操作

Docker 疑似クラスタ（`dev/compose/`）で代替できるものはそちらで行ってください。
実機でしか確認できない項目は [VM_VALIDATION.md](VM_VALIDATION.md) にまとめてあります。

---

## 8. SSH ポートが 22 でない場合

SSH を 22 以外で運用している場合、**設定しないと全 host が SSH 障害として報告されます。**
probe が閉じたポートを叩き、正しく「何もない」と報告するためです。

### 8.1 agent がいる host

**通常は何もしなくて構いません。**
agent が `/etc/ssh/sshd_config` を読み、`Port` / `ListenAddress host:port` から
実際のポートを検出して controller へ報告します。

確認:

```bash
sentinel doctor --json | python3 -c 'import json,sys; print(json.load(sys.stdin).get("hostname"))'
# controller 側で、報告されたポートを確認
sentinel entity show <hostname> --json | python3 -c 'import json,sys; print(json.load(sys.stdin))'
```

`sshd_config` が読めない、あるいはポートが別の場所で設定されている場合は
明示します。

```toml
[agent]
controller_address = "parent:7443"
ssh_port = 2222        # sshd_config から読めない場合のみ
```

優先順位は次のとおりです。

```text
[agent] ssh_port  >  sshd_config の Port  >  既定値 22
```

### 8.2 agent がいない host（設定で宣言する host）

自分で報告できないため、**必ず明示してください。**

```toml
[[entities]]
type = "host"
name = "filesrv01"
addresses = ["10.0.0.10"]
ports = { ssh = 2222 }
capabilities = ["storage.nfs.server", "observer.peer"]
```

### 8.3 host ごとにポートが異なる場合

`ports` は entity ごとの設定です。混在して構いません。

```toml
[[entities]]
type = "host"
name = "filesrv01"
ports = { ssh = 2222 }

[[entities]]
type = "host"
name = "filesrv02"
# 22 のまま。ports を書かない
```

### 8.4 確認方法

```bash
# 期待どおりのポートを叩いているか
sentinel entity show <hostname>       # ssh component が HEALTHY か
sentinel diagnose                     # SSH_SERVICE_FAILURE が出ていないか
```

`SSH_SERVICE_FAILURE` が全 host に出る場合、ポート設定を疑ってください。

---

## 9. その他の非標準構成

### 9.1 agent のポートを変える

既定は 7444 です。変更する場合:

```toml
[agent]
listen = "0.0.0.0:9444"
```

agent は自分のポートを registration で報告するため、
**controller 側に追記する必要はありません。**
peer も正しいポートを叩きます。

agent がいない host に対して指定する場合のみ:

```toml
ports = { agent = 9444 }
```

### 9.2 controller のポートを変える

```toml
# controller 側
[controller]
listen = "0.0.0.0:8443"

# agent 側
[agent]
controller_address = "parent:8443"
```

### 9.3 Slurm NodeName と hostname が異なる

**設定は不要です。** Sentinel は両者を別のものとして扱い、
`NodeHostName` で対応付けます。

### 9.4 Slurm の設定ファイルが標準の場所にない

```toml
[discovery.slurm]
enabled = true
scontrol_path = "/opt/slurm/bin/scontrol"
```

allowlist はファイル名で照合するため、
`/opt/slurm/bin/scontrol` は許可され、`/opt/scontrol/rm` は許可されません。

### 9.5 capability の自動検出が期待と違う

```bash
sentinel doctor    # 判定理由を確認
```

そのうえで上書きします。

```toml
[capabilities]
"storage.nfs.server" = "force"     # 検出結果によらず ON
"storage.smart"      = "disable"   # 検出結果によらず OFF
"storage.zfs"        = "enable"    # 検出が何も言わなかった場合に ON
```

優先順位:

```text
disable  >  force  >  runtime discovery  >  enable / role hint
```

### 9.6 TLS

TLS は組み込みです。reverse proxy は不要です。

平文のままでも動作しますが、cluster credential は bearer token なので、
wire を読める者は全 agent になりすませます。
隔離された管理 network 以外では TLS を設定してください。

#### 9.6.1 証明書の準備

既存の PKI で発行してください。Sentinel は証明書を生成しません
（監視システムが trust anchor を発行すると、誰も監査しない private CA が増えるだけです）。

controller の証明書には、**agent が接続に使う名前またはアドレス**を
SAN に入れてください。

```bash
# 例: 手元の CA で発行する場合
openssl x509 -req -in controller.csr -CA ca.crt -CAkey ca.key \
  -extfile <(printf "subjectAltName=DNS:controller.example,IP:10.0.0.10") \
  -days 825 -out controller.crt
```

配置とパーミッション:

```bash
install -d -m 0755 /etc/sentinel/tls
install -m 0644 ca.crt         /etc/sentinel/tls/ca.crt
install -m 0644 controller.crt /etc/sentinel/tls/controller.crt
install -m 0600 -o sentinel -g sentinel controller.key /etc/sentinel/tls/controller.key
```

#### 9.6.2 TLS のみ（server 認証）

```toml
# controller
[tls]
cert = "/etc/sentinel/tls/controller.crt"
key  = "/etc/sentinel/tls/controller.key"
```

```toml
# agent
[agent]
controller_address = "controller.example:7443"

[tls]
ca = "/etc/sentinel/tls/ca.crt"
```

`[tls]` に client 側の設定が 1 つでもあると、
`host:port` は `https://` として解釈されます。
`controller_address` に scheme を書いた場合はそちらが優先されます。

#### 9.6.3 mutual TLS（推奨）

**token が漏れても耐えられる構成はこれだけです。**
証明書を持たない client は token を出すことすらできません。

```toml
# controller
[tls]
cert      = "/etc/sentinel/tls/controller.crt"
key       = "/etc/sentinel/tls/controller.key"
client_ca = "/etc/sentinel/tls/ca.crt"     # これを書くと client 証明書は必須
```

```toml
# agent
[tls]
ca          = "/etc/sentinel/tls/ca.crt"
client_cert = "/etc/sentinel/tls/agent.crt"
client_key  = "/etc/sentinel/tls/agent.key"
```

agent 用の証明書は host ごとに発行してください
（1 枚を全 host で共有すると、1 台の侵害が全体の侵害になります）。

#### 9.6.4 IP アドレスで接続する場合

controller の証明書が名前しか持たず、agent が IP で接続する場合:

```toml
[agent]
controller_address = "10.0.0.10:7443"

[tls]
ca          = "/etc/sentinel/tls/ca.crt"
server_name = "controller.example"   # 証明書上の名前
```

`server_name` は「アドレスで接続するが、証明書上の名前で検証する」ための
設定です。`controller_address` が既に名前の場合は使えません（エラーになります）。

#### 9.6.5 PKI がまだ無い場合

```toml
[tls]
insecure_skip_verify = true
```

**これは TLS を装飾に変えます。** 接続を横取りできる攻撃者は
任意の証明書を提示でき、credential はそのまま読まれます。
起動のたびに警告が出ます。暫定措置としてのみ使ってください。

#### 9.6.6 確認

```bash
# 設定の妥当性（cert だけあって key が無い等はここで落ちる）
sentinel config check

# controller が TLS で listen しているか
journalctl -u sentinel-controller | grep "controller listening"
#   -> tls=true

# 証明書チェーンの確認
openssl s_client -connect controller.example:7443 \
  -CAfile /etc/sentinel/tls/ca.crt </dev/null
```

TLS 材料が読めない場合、controller は**起動に失敗します**。
平文で起動して「暗号化されている」と誤解されるのが最悪の失敗形だからです。

詳細は [SECURITY.md](SECURITY.md) と
[CONFIGURATION.md](CONFIGURATION.md) の `[tls]` を参照してください。

### 9.7 NIC が複数ある場合（VLAN・bridge・複数 fabric）

peer がこの host を probe するアドレスは、agent が自動検出します。
ただし **「どの NIC がクラスタ内通信を担っているか」は自動では分かりません。**
それは host の性質ではなく site の事実です。

```
$ ip -o addr show
1: lo       inet 127.0.0.1/8
2: eno8303  inet6 fe80::c6d6:d3ff:fe5c:8ec8/64
6: vlan32   inet 192.168.32.2/24
7: vlan20   inet 192.168.20.2/24     ← クラスタ内通信はこれ
8: vlan10   inet 192.168.10.2/24
9: wg0      inet 10.0.0.1/24
```

この host を調べても、`vlan20` が答えだと分かる手がかりはありません。
そのため Sentinel は **候補が複数あることを報告し、選択を求めます。**

```bash
sentinel doctor
```

```
Address:     192.168.10.2
  -> vlan10           192.168.10.2
     vlan20           192.168.20.2
     vlan32           192.168.32.2
     wg0              10.0.0.1
  ! several interfaces could be the one peers reach this host on
    (vlan10, vlan20, vlan32); 192.168.10.2 was chosen by name order.
    Set [agent] interface to say which.
```

**指定してください。** fleet 全体で同じ 1 行が使えます。

```toml
[agent]
interface = "vlan20"
```

NAT 越しなど host 自身から見えないアドレスの場合は直接指定します。

```toml
[agent]
address = "203.0.113.9"
```

agent がいない host は、これまでどおり `[[entities]]` の `addresses` で宣言します。

```toml
[[entities]]
type = "host"
name = "filesrv01"
addresses = ["192.168.20.30"]
```

#### 指定しないとどうなるか

「物理 NIC に見えるもののうち名前順で最初」が選ばれます。
上の例では `vlan10` です。**多くの場合これは間違いです。**

除外されるものは決まっています（ここは自動で正しく処理されます）。

* loopback アドレス（`127.0.0.0/8`、`::1`）
* link-local（`169.254.0.0/16`、`fe80::/10`）
* **`lo` インターフェース上の全アドレス** — WSL の `10.255.255.254/32` のように、
  loopback アドレスではないが誰からも到達できないもの
* `docker*` / `br-*` / `veth*` / `virbr*` / `wg*` / `tailscale*` などは後順位

#### 指定した NIC にアドレスが無い場合

**アドレスを報告しません。別の NIC にフォールバックしません。**
運用者が選ばなかったネットワークに peer 全員を向けるのが、
この設定で防ぎたい障害そのものだからです。

controller は host 名にフォールバックし、`doctor` が理由と実在する候補を表示します。

### 9.8 database の保持期間

controller の database は書き込み一方で、既定では
observation 14 日 / transition 90 日 / 解決済み incident 180 日で prune されます。
実測で host 1 台あたり 1 日約 170 MB 増えるため、
既定なら host あたり約 2.4 GB で頭打ちになります。

長期保存が必要な場合:

```toml
[retention]
observations       = "60d"
resolved_incidents = "never"     # incident は消さない
```

ディスクが小さい場合:

```toml
[retention]
observations = "3d"
interval     = "15m"
```

`sentinel prune --dry-run` で、実行前に削除量を確認できます。
運用手順は [OPERATIONS.md](OPERATIONS.md) を参照してください。

---

## 10. 導入後の確認

```bash
# 全 agent が登録されたか
curl -fsS http://localhost:7443/v1/health

# 全 entity が想定どおりか
sentinel status

# 依存関係が実構成と一致するか
sentinel dependency list

# observer が 3 台付いているか。付いていない entity は明示される
sentinel peers

# 誤検知が出ていないか
sentinel diagnose
```

### 導入直後に期待される状態

| 状態 | 意味 |
| --- | --- |
| 多くが `HEALTHY` | 正常 |
| agent 未導入の host が `UNKNOWN` | **正常。** 観測していないものを healthy とは呼びません |
| `sentinel diagnose` が空 | 正常 |
| `SLURM_ONLY_DEGRADATION` | 実際に DRAIN されている node があれば正常 |

### 誤検知が出た場合

| 症状 | 疑うところ |
| --- | --- |
| 全 host に `SSH_SERVICE_FAILURE` | SSH ポート（[§8](#8-ssh-ポートが-22-でない場合)） |
| fileserver に `SLURM_*` | 通常起きません。起きた場合は報告してください |
| 個別の `NFS_CLIENT_FAILURE` が多発 | storage の依存関係が未記述 |
| 到達性の診断が一切出ない | observer 不足（`sentinel peers`） |

---

## 11. 設定ファイルテンプレート

**通常は `sentinel install` または `sentinel config init` が生成するファイルを
使ってください。** 全設定が既定値のまま説明つきで書き出され、
書き換えが必要な行には `CHANGE-ME` が入っています。

```bash
sentinel config init --role controller --output /etc/sentinel/config.toml
sentinel config init --role agent      --output /etc/sentinel/config.toml
sentinel config init --role agent --dry-run   # 中身だけ見る
```

以下は、生成物を待たずに構成を先に検討したい場合の参考です。
同じものが [`docs/templates/`](templates/) にもあります。

### 11.1 Controller (`/etc/sentinel/config.toml`)

```toml
config_version = 1

# 全 host で一致させること
environment = "mizuno-lab"

[controller]
listen = "0.0.0.0:7443"
inventory_interval = "5m"
# controller 自身も観測点として動作する。
# firewall の内側にいて視界が偏る場合のみ false にする
observe = true

[database]
path = "/var/lib/sentinel/sentinel.db"

[peer_monitoring]
# 1 entity あたりの observer 数。
# 0 にすると到達性の診断ができなくなる
degree = 3

[discovery.slurm]
enabled = true
# scontrol が PATH にない場合のみ
# scontrol_path = "/opt/slurm/bin/scontrol"

[notification]
# 導入初期は "critical" にして様子を見るのも可
min_severity = "warning"

# [[notification.webhooks]]
# name = "ntfy"
# url  = "https://ntfy.example.org/cluster-sentinel"

# ---------------------------------------------------------------------------
# Scheduler entity
#
# Slurm の ClusterName に合わせておくと分かりやすい。
# 書かない場合は "slurm" になる。
# ---------------------------------------------------------------------------
[[entities]]
type = "scheduler"
name = "mizuno_cluster"

# ---------------------------------------------------------------------------
# Slurm の外にある host
#
# Slurm discovery では見つからないため、ここで宣言する。
# agent を入れる予定であっても、先に書いておいてよい（merge される）。
# ---------------------------------------------------------------------------
[[entities]]
type = "host"
name = "filesrv01"
addresses = ["10.0.0.10"]
capabilities = ["storage.nfs.server", "observer.peer"]
labels = { role = "fileserver", rack = "r01" }
# SSH が 22 以外の場合のみ
# ports = { ssh = 2222 }

[[entities]]
type = "host"
name = "filesrv02"
addresses = ["10.0.0.11"]
capabilities = ["storage.nfs.server", "observer.peer"]
labels = { role = "fileserver", rack = "r01" }

# ---------------------------------------------------------------------------
# Storage entity
#
# fileserver そのものとは別の概念として扱う。
# 「fileserver は生きているが export service だけ落ちた」を表現するために必要。
# ---------------------------------------------------------------------------
[[entities]]
type = "storage"
name = "filesrv01-storage"

[[entities]]
type = "storage"
name = "filesrv02-storage"

# ---------------------------------------------------------------------------
# 依存関係
#
# ここを書かないと、複数 node の storage 障害が
# SHARED_STORAGE_FAILURE（原因 = fileserver）ではなく
# 個別の NFS_CLIENT_FAILURE として報告される。
#
# from が to に依存する。
# ---------------------------------------------------------------------------
[[dependencies]]
from = "storage/filesrv01-storage"
to   = "host/filesrv01"
type = "provides"

[[dependencies]]
from = "storage/filesrv02-storage"
to   = "host/filesrv02"
type = "provides"

# filesrv01 を使う node
[[dependencies]]
from = "host/creator2"
to   = "storage/filesrv01-storage"
type = "uses_storage"

[[dependencies]]
from = "host/creator3"
to   = "storage/filesrv01-storage"
type = "uses_storage"

# filesrv02 を使う node
[[dependencies]]
from = "host/creator5"
to   = "storage/filesrv02-storage"
type = "uses_storage"

# ---------------------------------------------------------------------------
# capability の上書き（必要な場合のみ）
# ---------------------------------------------------------------------------
# [capabilities]
# "storage.nfs.server" = "force"
```

### 11.2 Agent (`/etc/sentinel/config.toml`)

**全 agent host で同じ内容で構いません。**
capability は自動検出されます。

```toml
config_version = 1

# controller と一致させること
environment = "mizuno-lab"

[agent]
controller_address = "parent:7443"
spool_path = "/var/lib/sentinel/spool.db"

# health endpoint。peer がここを見て
# 「agent だけ落ちた」と「host が落ちた」を区別する
listen = "0.0.0.0:7444"

# SSH が 22 以外で、かつ sshd_config から読めない場合のみ
# ssh_port = 2222

# UI 上のグループ分けにのみ使う。probe を有効化しない
roles = ["compute"]

# ---------------------------------------------------------------------------
# この host を peer observer にする場合
#
# observer は互いに異なる障害ドメインから選ぶこと。
# ---------------------------------------------------------------------------
[capabilities]
"observer.peer" = "force"
```

### 11.3 最小構成（動作確認用）

```toml
# controller
config_version = 1
environment = "mizuno-lab"

[controller]
listen = "0.0.0.0:7443"

[database]
path = "/var/lib/sentinel/sentinel.db"

[discovery.slurm]
enabled = true
```

```toml
# agent
config_version = 1
environment = "mizuno-lab"

[agent]
controller_address = "parent:7443"
```

---

## 12. チェックリスト

### 導入前

- [ ] environment 名を決めた
- [ ] controller を置く host を決めた
- [ ] observer にする host を 3 台以上決めた（異なる障害ドメイン）
- [ ] storage の依存関係を把握した
- [ ] SSH ポートを確認した（22 以外なら [§8](#8-ssh-ポートが-22-でない場合)）
- [ ] agent → controller の network 到達性を確認した

### 各 host

- [ ] `sentinel` を `/usr/local/bin` へ配置した
- [ ] `sentinel` ユーザーとディレクトリを作成した
- [ ] `/etc/sentinel/token` を配置した（0400、`sentinel` 所有）
- [ ] `/etc/sentinel/config.toml` を作成した
- [ ] `sentinel config check` が通った
- [ ] `sentinel install <role>` で unit を生成した
- [ ] サービスが起動し、`systemctl status` が正常
- [ ] `sentinel doctor` の capability が想定どおり

### 全体

- [ ] `curl .../v1/health` で全 agent が登録されている
- [ ] `sentinel status` が実構成と一致する
- [ ] `sentinel dependency list` が実 storage 構成と一致する
- [ ] `sentinel peers` で observer の付いていない entity がない
- [ ] `sentinel diagnose` に誤検知がない
- [ ] 数日おいて誤検知が出ないことを確認した
- [ ] notification をテスト宛先で確認した
