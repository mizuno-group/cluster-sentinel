# 実クラスタ導入マニュアル

実際の計算クラスタへ Cluster Sentinel を導入する手順書です。
**網羅性を優先しているので長くなっています。**

> **はじめて触る場合は [GETTING_STARTED.md](GETTING_STARTED.md) から
> 読んでください。** 30 分で動くところまで行けます。
> この文書は、そのあと本番構成へ広げるときに読むものです
> （TLS、非標準ポート、複数 NIC、段階的導入など）。

**Rust ツールチェインは不要です。** 配布物は静的リンクされた単一バイナリで、
[Releases](https://github.com/Lzh-Function/cluster-sentinel/releases) から
取得したものをコピーするだけです。

前提として、**Sentinel は監視対象を一切変更しません。**
reboot・`systemctl restart`・mount 操作・`scontrol update` を実行しません。
したがって導入自体がクラスタの動作を変えることはありませんが、
段階的に入れることを強く推奨します（[§7](#7-段階的導入)）。

日常運用は [OPERATIONS.md](OPERATIONS.md)、
設定項目の網羅的な説明は [CONFIGURATION.md](CONFIGURATION.md) を参照してください。

---

## 目次

| | 節 | 対象 |
| --- | --- | --- |
| 1 | [事前確認](#1-事前確認) | — |
| 2 | [構成の決定](#2-構成の決定) | — |
| 3 | [バイナリの配置](#3-バイナリの配置) | controller と agent を置く全 host |
| 4 | [Controller の構築](#4-controller-の構築) | controller の host |
| 5 | [cluster credential の配布](#5-cluster-credential-の配布) | 全 host |
| 6 | [Agent の展開](#6-agent-の展開) | agent を置く各 host |
| 6.7 | [監視頻度を変える](#67-監視頻度を変える) | 任意 |
| 7 | [段階的導入](#7-段階的導入) | — |
| 8 | [SSH ポートが 22 でない場合](#8-ssh-ポートが-22-でない場合) | 該当する場合 |
| 9 | [その他の非標準構成](#9-その他の非標準構成) | 該当する場合 |
| 9.7 | [NIC が複数ある場合](#97-nic-が複数ある場合vlanbridge複数-fabric) | **VLAN 環境は必読** |
| 9.9 | [ストレージ構成は書かなくてよい](#99-ストレージ構成は書かなくてよい例外は-3-つ) | NFS を使う場合 |
| 9.10 | [controller に agent を同居させる](#910-controller-に-agent-を同居させる) | 推奨 |
| 10 | [導入後の確認](#10-導入後の確認) | — |
| 11 | [設定ファイルテンプレート](#11-設定ファイルテンプレート) | 参考 |
| 12 | [チェックリスト](#12-チェックリスト) | — |

---

## 1. 事前確認

### 必要なもの

| 項目 | 内容 |
| --- | --- |
| OS | Linux（systemd 前提） |
| 権限 | 各 host の root（導入時のみ。常駐は非特権ユーザー） |
| network | agent → controller への TCP 到達性（既定 7443） |
| | controller / peer → 各 host への TCP 到達性（SSH ポート、agent ポート 7444） |

**Rust ツールチェインは不要です。** 配布されるのは静的リンクされた
単一バイナリで、ビルド済みのものをコピーするだけです。

Sentinel は Slurm を **変更しません**。既存の `slurm.conf` を書き換える必要はありません。

### どの host に何を置くか

導入前にこの表を埋めてください。以降の手順はこれに沿って進みます。

| host | 役割 | 置くもの |
| --- | --- | --- |
| 1 台 | **controller** | バイナリ + 設定 + unit + credential（生成元） |
| 監視したい host | **agent** | バイナリ + 設定 + unit + credential（controller からコピー） |
| agent を置かない host | 外から観測されるだけ | **何も置きません**（controller の設定に宣言するだけ） |

**agent を置かない host にはバイナリも設定ファイルも要りません。**
controller と peer が外から TCP で観測します。
ただし取得できるのは到達性・SSH・NFS ポートまでで、
load・memory・GPU・kernel event などその host の内側は一切見えません。

### 確認しておく情報

```bash
# controller にする host の名前と、agent から到達できるアドレス
hostname -f
ip -o addr show

# アーキテクチャ（x86_64 と ARM が混在するクラスタでは host ごとに確認）
uname -m

# Slurm の controller と node 定義（あれば）
grep -E "^(SlurmctldHost|ControlMachine|NodeName)" /etc/slurm/slurm.conf

# SSH のポート（22 以外なら §8 を参照）
grep -iE "^\s*(Port|ListenAddress)" /etc/ssh/sshd_config

# Slurm の外にある host（fileserver 等）の一覧
#   NFS のマウント関係は agent の報告から自動で導出されるため、
#   調べておく必要はない（§9.9）
```

---

## 2. 構成の決定

| 決めること | 例 | 備考 |
| --- | --- | --- |
| environment 名 | `example-lab` | 全 host で一致させる |
| controller を置く host | `head01` | source code には現れない。設定だけの問題 |
| scheduler entity 名 | `example_cluster` | Slurm の ClusterName に合わせると分かりやすい |
| observer にする host | controller / fileserver / 一部 compute | **3 台以上**を推奨（後述） |
| クラスタ内通信の NIC | `vlan102` など | NIC が複数あるなら必須（§9.7） |
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

**controller と agent を置く全 host** で行います。
agent を置かない host には不要です。

### 3.1 ダウンロード

[Releases](https://github.com/Lzh-Function/cluster-sentinel/releases) から、
その host のアーキテクチャに合うものを取得します。

```bash
# x86_64
curl -fsSLO https://github.com/Lzh-Function/cluster-sentinel/releases/latest/download/sentinel-x86_64-unknown-linux-musl
curl -fsSLO https://github.com/Lzh-Function/cluster-sentinel/releases/latest/download/sentinel-x86_64-unknown-linux-musl.sha256
sha256sum -c sentinel-x86_64-unknown-linux-musl.sha256

# ARM64
curl -fsSLO https://github.com/Lzh-Function/cluster-sentinel/releases/latest/download/sentinel-aarch64-unknown-linux-musl
curl -fsSLO https://github.com/Lzh-Function/cluster-sentinel/releases/latest/download/sentinel-aarch64-unknown-linux-musl.sha256
sha256sum -c sentinel-aarch64-unknown-linux-musl.sha256
```

`uname -m` が `x86_64` なら前者、`aarch64` なら後者です。

### 3.2 配置

```bash
sudo install -m 0755 sentinel-x86_64-unknown-linux-musl /usr/local/bin/sentinel
sentinel version
```

```
sentinel 1.0.0
protocol version: 1
config version:   1
target:           x86_64-unknown-linux-musl
```

> 初回はこれで構いませんが、**すでに Sentinel が動いているホストを
> 更新するときは `install` ではなく `mv` を使ってください。**
> 実行中のバイナリは truncate できず `Text file busy` になります
> （[OPERATIONS.md のアップグレード](OPERATIONS.md#アップグレード)）。

**必ず `/usr/local/bin` に置いてから次に進んでください。**
ダウンロードしたディレクトリのまま `sentinel install` を実行すると、
生成される systemd unit がそのパスを指し、ディレクトリを片付けた時点で
サービスが起動しなくなります。`install` はこの状態を警告します。

静的リンクなので、glibc のバージョンや配布物の追加は不要です。

```bash
ldd /usr/local/bin/sentinel      # -> statically linked
```

同一アーキテクチャなら全 host に同じファイルを配れます。

```bash
# 例: 各 compute node へ配る
for n in node01 node02 node03; do
  scp sentinel-x86_64-unknown-linux-musl "$n":/tmp/sentinel
  ssh "$n" 'sudo install -m 0755 /tmp/sentinel /usr/local/bin/sentinel && rm /tmp/sentinel'
done
```

---

## 4. Controller の構築

**controller にする host 1 台**で行います。

### 4.1 サービスユーザーを作る

`install` より先に作ってください。生成されるファイルの所有者になります。

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel
```

`/etc/sentinel` も `/var/lib/sentinel` も、**この時点では存在しなくて構いません。**
前者は `install` が、後者は systemd が起動時に作ります。

### 4.2 install

```bash
sudo sentinel install controller
```

これ 1 回で、ディレクトリごと以下が生成されます。

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

### 4.3 所有者を合わせる

`install` は root として書くので、サービスユーザーに渡します。

```bash
sudo chown -R sentinel:sentinel /etc/sentinel
```

`/var/lib/sentinel`（database の置き場）は unit の `StateDirectory=` により
**systemd が初回起動時に作成し、所有者も設定します。** 手で作る必要はありません。

### 4.4 設定の仕上げ

生成された `/etc/sentinel/config.toml` のうち、
**書き換えが必要なのは `CHANGE-ME` を含む行だけ**です。
controller の場合は `environment` の 1 行です。

それ以外はすべて既定値がそのまま書き出されており、
変更したい行のコメントを外すか値を書き換えます。
Slurm の外にある fileserver や依存関係は、
ファイル内のコメント例を参考にしてください（§11 にも同じものがあります）。

### 4.5 検証

**起動前に必ず実行してください。**

```bash
sudo -u sentinel sentinel config check
```

検出できる問題をすべて報告します（最初の 1 件で止まりません）。
`error` が 1 つでもあれば起動しません。

`warning` は許容されます。特に
「宣言されていない entity への依存」は、
Slurm discovery や agent registration から到着する予定のものであれば正常です。

### 4.6 起動

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now sentinel-controller
systemctl status sentinel-controller
journalctl -u sentinel-controller -f
```

### 4.7 動作確認

**CLI は root か `sentinel` ユーザーで実行してください。**

```bash
sudo -u sentinel sentinel status     # 推奨
sudo sentinel status
```

一般ユーザーで実行すると設定ファイルを読めません。
設定ファイルは world-readable にしていません
（`[[notification.webhooks]]` の URL 自体が credential を含みうるため）。

```
$ sentinel status
error: cannot read config file /etc/sentinel/config.toml: permission denied.
The configuration belongs to the service user, so run one of:
  sudo -u sentinel sentinel <command>
  sudo sentinel <command>
```


```bash
curl -fsS http://localhost:7443/v1/health
sudo -u sentinel sentinel status
sudo -u sentinel sentinel dependency list
```

**この時点で entity は 0 件です。これは正常です。**

```
ENVIRONMENT: example-lab

No entities known yet.
```

entity が現れる経路は 2 つあり、どちらもまだ動いていないためです。

**1. Slurm discovery。** `install` は `scontrol` の有無を見て
`[discovery.slurm] enabled` を設定します。この host に `scontrol` が
無かった場合は `false` になっているので、Slurm クラスタなら手で `true` にします。

```bash
sudo -u sentinel grep -A3 "discovery.slurm" /etc/sentinel/config.toml
```

```toml
[discovery.slurm]
enabled = true
```

有効にしたら、次の inventory 周期（既定 5 分）を待たずに実行できます。

```bash
sudo systemctl restart sentinel-controller
sudo -u sentinel sentinel discover
sudo -u sentinel sentinel status
```

**2. agent の登録。** §6 で展開すると現れます。

Slurm を使っていない、あるいは Slurm の外にある host は
`[[entities]]` で宣言します（§11.1 のテンプレート参照）。

agent がいない entity が `UNKNOWN` と出るのも正常です。
**観測していないものを healthy とは呼びません。**

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

`sentinel install agent` は credential を生成しません（§6.1）。

---

## 6. Agent の展開

**agent を置く各 host** で行います。以下はすべてその host 上での作業です。

前提は「§3 でバイナリを `/usr/local/bin/sentinel` に置いた」ことだけです。
`/etc/sentinel` は存在しなくて構いません。`install` が作ります。

### 6.1 サービスユーザーと install

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel
sudo sentinel install agent
sudo chown -R sentinel:sentinel /etc/sentinel
```

生成物は 2 つです。

| 生成物 | 内容 |
| --- | --- |
| `/etc/sentinel/config.toml` | 全設定を既定値のまま書き出した設定ファイル |
| `/etc/systemd/system/sentinel-agent.service` | hardening 済み systemd unit |

**credential は生成されません。** controller のものを配ります（§5）。
agent が自前で生成すれば、クラスタの誰も知らない credential ができてしまい、
「設定の問題」が「認証の失敗」として現れることになるためです。

`/var/lib/sentinel`（spool の置き場）は systemd が初回起動時に作ります。

### 6.2 credential を置く

§5 で controller から配ったものを配置します。

```bash
sudo install -o sentinel -g sentinel -m 0400 /path/to/token /etc/sentinel/token
```

### 6.3 設定を書き換える

`CHANGE-ME` を含む **2 行**だけです。

```toml
environment = "CHANGE-ME-environment"                 # controller と一致させる
controller_address = "CHANGE-ME-controller-host:7443" # controller のアドレス
```

capability は agent が自動検出するため、列挙する必要はありません。

**NIC が複数ある host では、もう 1 行必要です**（§9.7）。
まず候補を確認します。

```bash
sudo sentinel doctor
```

候補が複数あると警告が出るので、`[agent]` セクションに追記します。

```toml
[agent]
interface = "vlan102"
```

### 6.4 検証と起動

```bash
sudo -u sentinel sentinel config check
sudo systemctl daemon-reload
sudo systemctl enable --now sentinel-agent
systemctl status sentinel-agent
```

### 6.5 確認

```bash
# この host が Sentinel からどう見えるか、capability と報告アドレスの判定理由つき
sudo sentinel doctor

# controller 側から
sentinel entity show <hostname>
sentinel status
```

`sentinel doctor` は capability ごとに
「detected on this host」「not present on this host」
「forced on by configuration」「suggested by a role」
のいずれかを表示します。想定と違う場合はここで分かります。

報告アドレスの行に `!` の警告が残っていないことも確認してください。

### 6.6 まとめて展開する場合

**ノードが 10 台を超えるなら [Ansible ロール](../deploy/ansible/) を使ってください。**
§3〜§6 をそのまま自動化してあり、アーキテクチャ別のバイナリ取得・
チェックサム検証・credential 配布・`config check`・起動まで行います。

```bash
cd deploy/ansible
cp inventory.example.ini inventory.ini
$EDITOR inventory.ini
ansible-playbook -i inventory.ini site.yml --limit node01   # まず 1 台
ansible-playbook -i inventory.ini site.yml
```

SSH と `sudo` にパスワードが必要な場合:

```bash
ansible-playbook -i inventory.ini site.yml --ask-pass --ask-become-pass
```

`--ask-pass` には `sshpass` が必要です。**SSH は鍵にしておくことを推奨します**
（`ssh-copy-id` を 1 回。パスワード認証は毎タスクで使われ、
`PasswordAuthentication no` の環境では使えません）。
詳細は [deploy/ansible/README.md](../deploy/ansible/README.md) を参照してください。

Sentinel 側に Ansible 固有のものはありません。別の構成管理ツールなら、
同じ手順（バイナリを置く → `sentinel install agent` → 設定と credential を配る）
を移植してください。

#### 手作業で配る場合

```bash
for n in node01 node02 node03; do
  scp sentinel-x86_64-unknown-linux-musl "$n":/tmp/sentinel
  scp /etc/sentinel/token "$n":/tmp/token
  ssh "$n" '
    sudo install -m 0755 /tmp/sentinel /usr/local/bin/sentinel
    sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel 2>/dev/null || true
    sudo sentinel install agent
    sudo install -o sentinel -g sentinel -m 0400 /tmp/token /etc/sentinel/token
    sudo chown -R sentinel:sentinel /etc/sentinel
    rm -f /tmp/sentinel /tmp/token
  '
done
```

このあと各 host の `config.toml` を書き換えます
（`environment`、`controller_address`、必要なら `interface`）。
3 行とも全 host で同じ値になるなら、書き換えた 1 つを配って構いません。

```bash
for n in node01 node02 node03; do
  scp /etc/sentinel/config.toml "$n":/tmp/config.toml
  ssh "$n" '
    sudo install -o sentinel -g sentinel -m 0640 /tmp/config.toml /etc/sentinel/config.toml
    sudo -u sentinel sentinel config check
    sudo systemctl daemon-reload && sudo systemctl enable --now sentinel-agent
    rm -f /tmp/config.toml
  '
done
```

> `scp` の経由地にファイルを残さないでください。credential も設定も
> `/tmp` を通ります。

---

## 6.7 監視頻度を変える

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
controller_address = "head01:7443"
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
controller_address = "head01:8443"
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

### 9.6.5 firewall で開けるポート

peer 同士が観測しあうため、**ノード間**で以下が通る必要があります。

| ポート | 用途 | 開けないとどうなるか |
| --- | --- | --- |
| SSH のポート（22 とは限らない） | 到達性 probe と SSH probe | host が到達不能に見える |
| **7444** | agent の health endpoint | `agent` component が UNAVAILABLE のまま |
| 7443（→ controller のみ） | agent からの報告 | agent が登録できない |

**DROP ではなく REJECT にするか、明示的に許可してください。**
DROP された場合、probe は timeout と区別できません。

到達性 probe は**設定済みの SSH ポート**を叩きます（22 固定ではありません）。
agent が `sshd_config` から自動検出して報告するので、通常は設定不要です。

### 9.7 NIC が複数ある場合（VLAN・bridge・複数 fabric）

peer がこの host を probe するアドレスは、agent が自動検出します。
ただし **「どの NIC がクラスタ内通信を担っているか」は自動では分かりません。**
それは host の性質ではなく site の事実です。

```
$ ip -o addr show
1: lo       inet 127.0.0.1/8
2: eno1  inet6 fe80::5054:ff:fe12:3456/64
6: vlan103   inet 192.0.2.32/24
7: vlan102   inet 192.0.2.20/24     ← クラスタ内通信はこれ
8: vlan101   inet 192.0.2.10/24
9: wg0      inet 10.0.0.1/24
```

この host を調べても、`vlan102` が答えだと分かる手がかりはありません。
そのため Sentinel は **候補が複数あることを報告し、選択を求めます。**

```bash
sentinel doctor
```

```
Address:     192.0.2.10
  -> vlan101           192.0.2.10
     vlan102           192.0.2.20
     vlan103           192.0.2.32
     wg0              10.0.0.1
  ! several interfaces could be the one peers reach this host on
    (vlan101, vlan102, vlan103); 192.0.2.10 was chosen by name order.
    Set [agent] interface to say which.
```

**指定してください。** fleet 全体で同じ 1 行が使えます。

```toml
[agent]
interface = "vlan102"
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
addresses = ["192.0.2.30"]
```

#### 指定しないとどうなるか

「物理 NIC に見えるもののうち名前順で最初」が選ばれます。
上の例では `vlan101` です。**多くの場合これは間違いです。**

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

### 9.9 ストレージ構成は書かなくてよい（例外は 3 つ）

**NFS の依存関係を設定ファイルに書く必要はありません。**

agent は毎サイクル自分のマウント表を報告しています。controller はそこから
fileserver の host entity、storage entity、`provides`、`uses_storage` を
すべて組み立てます。ノードを増やしても、マウント先を変えても、
設定ファイルは触りません。

```bash
sentinel discover && sentinel dependency list
```

止めたい場合は `[discovery.nfs] enabled = false` です。

#### 手で書く必要がある 3 つの場合

**1. マウントが IP で書かれていて、その IP を持つ host を Sentinel が知らない**

`10.0.0.9:/data` のようなマウントは、その address を登録している host が
いれば自動で結び付きます。いなければ **entity は作られません**。
address は identity ではないため（[ADR 0001](adr/0001-deterministic-entity-identity.md)）、
`10.0.0.9` という名前の entity を捏造すると、そのマシンが後から自分の名前で
登録したときに**同じマシンに 2 つの identity ができて**しまうからです。

黙って落とすことはせず、`sentinel discover` が報告します。

```
NFS mounts that could not be tied to a known host:
  10.0.0.9  mounted by node01, node02
```

その host を宣言すれば、以降は自動で解決されます。

```toml
[[entities]]
type = "host"
name = "the-fileserver"
addresses = ["10.0.0.9"]
```

**2. NFS 以外の共有ストレージ**

Lustre、GPFS、オブジェクトストレージなど。導出は NFS のマウント表を
見ているだけなので、それ以外は従来どおり宣言します。

```toml
[[entities]]
type = "storage"
name = "lustre-scratch"

[[dependencies]]
from = "storage/lustre-scratch"
to   = "host/mds01"
type = "provides"

[[dependencies]]
from = "host/node01"
to   = "storage/lustre-scratch"
type = "uses_storage"
```

**3. どのノードもマウントしていないが監視したい fileserver**

誰もマウントしていなければマウント表に現れないため、導出されません。

手で書いた宣言は導出結果と**併存**します。打ち消し合いません。

#### storage entity の健全性はどこから来るか

storage entity には probe を打つ相手がいません（それは「概念」であって
マシンではないため）。健全性は**提供元ホストの export 検査**から導かれます。

ここで使うのは **server 側の検査だけ**です。1 台のホストが fileserver でも
NFS クライアントでもありうるので（scratch を export しつつ他所の home を
マウントする計算ノード）、両者を混ぜると**クライアント側のマウント詰まりが
「このホストの export が壊れた」として報告され**、人を間違ったマシンに
送ることになります。

### 9.10 controller に agent を同居させる

**推奨します。** controller のホスト（多くはヘッドノード）に agent を
入れていないと、そのホストは**外から到達性を見られるだけ**になります。
CPU もメモリも NFS マウントも journal も見えません。
`slurmctld` が乗っている、いちばん落ちてほしくないマシンが
いちばん手薄になります。

同居させるときは **設定ファイルのパスを分けます**。既定のままだと
agent の install が controller の `config.toml` を上書きします。

```bash
sudo sentinel install agent --config /etc/sentinel/agent.toml
```

これで衝突しません。

| | controller | agent |
| --- | --- | --- |
| 設定 | `/etc/sentinel/config.toml` | `/etc/sentinel/agent.toml` |
| unit | `sentinel-controller.service` | `sentinel-agent.service` |
| ポート | 7443 | 7444 |
| 状態ファイル | `sentinel.db` | `spool.db` |
| credential | `/etc/sentinel/token`（**共用**） | 同左 |

credential は設定ファイルの隣を見に行くので、同じディレクトリに置く限り
自動的に共用されます。**agent の install が credential を作ったり
入れ替えたりすることはありません。**

書き換えるのは 2 行です。

```bash
sudo sed -i 's/^environment = .*/environment = "my-cluster"/; s/^controller_address = .*/controller_address = "127.0.0.1:7443"/' /etc/sentinel/agent.toml
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now sentinel-agent
```

同居させると、そのホストについて次が自動的に解決します。

- SSH ポートが 22 以外でも、agent が `sshd_config` から読んで報告する
  （§8 の手作業が不要になる）
- そのホストの NFS マウントが依存グラフに入る（§9.9）
- CPU・メモリ・journal・systemd サービスが見えるようになる

> **Ansible の `agents` グループには入れないでください。**
> ロールは `/etc/sentinel/config.toml` に書き込むため、
> **controller の設定が上書きされて controller が止まります。**
> このホストだけは上の手順で個別に入れてください。

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
environment = "example-lab"

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
name = "example_cluster"

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
# Storage entity と依存関係
#
# **NFS については、書く必要がありません。**
# agent が報告するマウント表から、fileserver の host entity、storage entity、
# provides、uses_storage がすべて自動で導出される。
# ノードがマウント先を変えても、この設定ファイルを触る必要はない。
#
#   確認:  sentinel dependency list
#   停止:  [discovery.nfs] enabled = false
#
# 手で書く必要があるのは §9.9 に挙げた 3 つの例外だけ。
# 手で書いたものは導出結果と併存する（打ち消し合わない）。
# ---------------------------------------------------------------------------

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
environment = "example-lab"

[agent]
controller_address = "head01:7443"
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
environment = "example-lab"

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
environment = "example-lab"

[agent]
controller_address = "head01:7443"
```

---

## 12. チェックリスト

### 導入前

- [ ] environment 名を決めた
- [ ] controller を置く host を決めた
- [ ] agent を置く host と、置かない host を決めた
- [ ] observer にする host を 3 台以上決めた（異なる障害ドメイン）
- [ ] 各 host のアーキテクチャを確認した（`uname -m`。x86_64 / ARM 混在なら host ごと）
- [ ] クラスタ内通信の NIC を確認した（複数あるなら [§9.7](#97-nic-が複数ある場合vlanbridge複数-fabric)）
- [ ] storage の依存関係を把握した
- [ ] SSH ポートを確認した（22 以外なら [§8](#8-ssh-ポートが-22-でない場合)）
- [ ] agent → controller の network 到達性を確認した

### Controller の host

- [ ] release からバイナリを取得し、`sha256sum -c` が通った
- [ ] `/usr/local/bin/sentinel` に配置し、`sentinel version` が動いた
- [ ] `sentinel` ユーザーを作成した（`install` より先に）
- [ ] `sudo sentinel install controller` を実行した
- [ ] `chown -R sentinel:sentinel /etc/sentinel` した
- [ ] `config.toml` の `CHANGE-ME` を書き換えた（`environment`）
- [ ] Slurm の外にある host と storage 依存を宣言した
- [ ] `sentinel config check` が通った
- [ ] サービスが起動し、`systemctl status` が正常
- [ ] `curl .../v1/health` が応答した

### Agent を置く各 host

- [ ] そのアーキテクチャ用のバイナリを `/usr/local/bin/sentinel` に配置した
- [ ] `sentinel` ユーザーを作成した（`install` より先に）
- [ ] `sudo sentinel install agent` を実行した（**警告が出ていないこと**)
- [ ] controller の `/etc/sentinel/token` を配置した（0400、`sentinel` 所有）
- [ ] `chown -R sentinel:sentinel /etc/sentinel` した
- [ ] `config.toml` の `CHANGE-ME` 2 行を書き換えた
- [ ] `sentinel doctor` の報告アドレスに `!` の警告がない（あれば `interface` を指定）
- [ ] `sentinel doctor` の capability が想定どおり
- [ ] `sentinel config check` が通った
- [ ] サービスが起動し、`systemctl status` が正常

### agent を置かない host

- [ ] controller の `config.toml` に `[[entities]]` として宣言した
- [ ] SSH ポートが 22 以外なら `ports = { ssh = ... }` を書いた
- [ ] IP を `addresses` で明示した

### 全体

- [ ] `curl .../v1/health` で全 agent が登録されている
- [ ] `sentinel status` が実構成と一致する
- [ ] `sentinel dependency list` が実 storage 構成と一致する
- [ ] `sentinel peers` で observer の付いていない entity がない
- [ ] `sentinel diagnose` に誤検知がない
- [ ] 数日おいて誤検知が出ないことを確認した
- [ ] notification をテスト宛先で確認した
