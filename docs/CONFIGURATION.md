# 設定リファレンス

Sentinel は TOML ファイルを 1 つ読み込みます。既定値は
`/etc/sentinel/config.toml`（`--config`、または環境変数 `SENTINEL_CONFIG` で変更可）。

何かを再起動する前に検証してください。

```bash
sentinel config check
```

最初の 1 件で止まらず、検出できた問題をすべて報告します。
設定が使用不能な場合は非 0 で終了します。
`sentinel config show` は実効設定を出力します。

## 設定ファイルの作り方

手で書き起こす必要はありません。

```bash
sentinel config init --role controller --output /etc/sentinel/config.toml
sentinel config init --role agent      --output /etc/sentinel/config.toml
```

全設定が既定値のまま、説明つきで書き出されます。
書き換えが必要なのは `CHANGE-ME` を含む行だけです。

`sentinel install` は設定ファイル・systemd unit・credential をまとめて生成します。
詳細は [DEPLOYMENT.md](DEPLOYMENT.md) を参照してください。

## 優先順位

```text
CLI  >  環境変数  >  設定ファイル  >  runtime discovery  >  built-in default
```

解決された各値は、どの層から供給されたかを記憶しています。

## `config_version`

必須。本ビルドが理解するのは version `1` です。

**より新しい** version を宣言したファイルは、部分的に読み込むのではなく拒否します。
理解できない設定を中途半端に適用することは、
「運用者が記述した対象とは別のものを監視する」ことを意味するためです。

```toml
config_version = 1
```

## 最小の agent 設定

```toml
config_version = 1
environment = "example-lab"

[agent]
controller_address = "controller.example:7443"
```

controller address に既定値はありません。
バイナリへ host 名を埋め込むことはしません。

## 最小の controller 設定

```toml
config_version = 1
environment = "example-lab"

[controller]
listen = "0.0.0.0:7443"

[database]
path = "/var/lib/sentinel/sentinel.db"

[peer_monitoring]
degree = 3

[discovery.slurm]
enabled = true
```

## セクション

### `[controller]`

| キー | 型 | 既定値 | 意味 |
| --- | --- | --- | --- |
| `listen` | `host:port` | `0.0.0.0:7443` | controller の待受アドレス |
| `inventory_interval` | duration | `5m` | inventory discovery の実行間隔（`scontrol` 実行・全 host probe を伴う） |
| `diagnosis_interval` | duration | `15s` | 診断・相関・通知の実行間隔 |

`diagnosis_interval` は **障害発生から通知までの遅延を決める値**です。
保存済みデータを読むだけなので安価であり、
高価な inventory discovery とは分けてあります。

### `[agent]`

| キー | 型 | 既定値 | 意味 |
| --- | --- | --- | --- |
| `controller_address` | `host:port` | *(なし)* | 報告先 controller |
| `spool_path` | path | `<state dir>/spool.db` | ローカル observation spool |
| `roles` | 文字列配列 | `[]` | grouping と default 提示のみに使うラベル |
| `listen` | `host:port` | `0.0.0.0:7444` | health endpoint の待受。peer がここを見る |
| `ssh_port` | 整数 | *(sshd_config から検出)* | SSH ポート。自動検出できない場合のみ |

`ssh_port` の優先順位:

```text
[agent] ssh_port  >  /etc/ssh/sshd_config の Port  >  既定値 22
```

通常は agent が `sshd_config` を読んで検出し、controller へ報告するため、
指定は不要です。`listen` を変更した場合も agent が自分で報告するため、
controller 側への追記は必要ありません。

`roles` は probe を有効化しません。後述の「Capability」を参照してください。

#### アドレスの決まり方

peer がこの host を probe するアドレスは、次の順で決まります。

| 優先 | 設定 | 用途 |
| --- | --- | --- |
| 1 | `[agent] address` | アドレスを直接指定。NAT 越しなど、host 自身から見えない場合 |
| 2 | `[agent] interface` | NIC 名で指定。**fleet 全体で同じ 1 行が使えるので推奨** |
| 3 | 自動検出 | 下記の規則で順位付け |

自動検出は次を行います。

1. **到達不能なものを除外** — loopback アドレス、link-local（`169.254.0.0/16`、
   `fe80::/10`）、および **`lo` インターフェース上の全アドレス**。
   `lo` に付いた非 loopback アドレス（WSL の `10.255.255.254/32` など）は
   誰からも到達できません
2. **物理 NIC を仮想 NIC より優先** — `docker*` / `br-*` / `veth*` / `virbr*` /
   `wg*` / `tailscale*` などは後ろに回します
3. IPv4 を IPv6 より優先し、以降は名前順（再起動しても順序が変わらないように）

**自動検出は「どの NIC がクラスタ内通信を担っているか」を答えられません。**
それは host の性質ではなく site の事実です。
`vlan101` / `vlan102` / `vlan103` を持つ host では、どれも同じくらい妥当に見えます。

そのため、**物理 NIC の候補が 2 つ以上ある場合は「曖昧である」と報告します。**
黙って 1 つ選ぶと、間違っていても気づけないためです。
`sentinel doctor` が候補一覧とともに表示します。

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

`interface` で指定した NIC に使えるアドレスが無い場合、
**アドレスを報告しません**（別の NIC にフォールバックしません）。
運用者が選ばなかったネットワークに peer を向けるのが、この設定で防ぎたい障害だからです。
その場合 controller は host 名にフォールバックし、`doctor` が理由を表示します。

### `[database]`

| キー | 型 | 既定値 |
| --- | --- | --- |
| `path` | path | `/var/lib/sentinel/sentinel.db` |

### `[probes]`

監視頻度を probe ごとに変更します。probe id をキーにしたテーブルです。

```toml
[probes."network.tcp"]
interval = "10s"

[probes."gpu.nvidia"]
enabled = false
```

| キー | 型 | 意味 |
| --- | --- | --- |
| `interval` | duration | 実行間隔 |
| `timeout` | duration | 1 回あたりの上限時間 |
| `max_outstanding` | 整数 | 同一 target への同時実行数（**引き下げのみ可能**） |
| `enabled` | bool | `false` でその probe を停止 |

**書かれていない probe は既定のまま動きます。** 1 つだけ調整しても他には影響しません。
未設定のキーも同様に既定値のままです。

既定値（`sentinel config init` が生成する設定ファイルにも全件書き出されます）:

| Probe | interval | timeout | 備考 |
| --- | --- | --- | --- |
| `network.tcp` | 5s | 3s | 到達性診断の土台 |
| `sentinel.agent` | 5s | 3s | remote のみ |
| `systemd.unit` | 10s | 5s | |
| `host.metrics` | 15s | 5s | |
| `ssh.service` | 15s | 5s | |
| `gpu.nvidia` | 15s | 10s | |
| `nfs.server.port` | 15s | 5s | |
| `nfs.client.mount` | 30s | 5s | `/proc` のみ |
| `nfs.client.io` | 30s | 10s | 同時実行 1（固定） |
| `journal.events` | 30s | 10s | 同時実行 1（固定） |
| `nfs.server.exports` | 60s | 5s | |

**`max_outstanding` は引き下げしかできません。**
`nfs.client.io` と `journal.events` は 1 に固定されています。
blocking syscall が積み上がらないようにするためであり
（`SPEC.md` §76、設計原則10）、設定ファイルで覆せません。

存在しない probe id を書くと **error** になります。
黙って無視されると「変更したつもりで変わっていない」状態になるためです。

override は controller の remote probe と agent の peer probe にも同じく適用されます。
観測者ごとに頻度が違うと、quorum が異なる頻度の観測を比較することになるためです。

### `[retention]`

記録したデータをどれだけ保持するかです。

| キー | 型 | 既定値 | 意味 |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | prune を行うか |
| `interval` | duration | `1h` | prune の実行間隔（起動時にも 1 回実行） |
| `observations` | period | `14d` | observation の保持期間 |
| `keep_per_entity` | 整数 | `64` | 期間に関わらず entity ごとに残す observation 数 |
| `transitions` | duration | `90d` | state transition の保持期間 |
| `resolved_incidents` | period | `180d` | **解決済み** incident の保持期間 |
| `diagnoses` | period | `30d` | どの incident にも属さない diagnosis の保持期間 |

period には duration（`"14d"`、`"6h"`）のほか、
`"never"`（`"forever"` / `"unlimited"` / `"keep"` も同義）を指定できます。

**なぜ class ごとに分かれているか。** バイト単価あたりの価値が違うからです。
observation は容量の大半を占め、個々の価値は最も低い
（昨日の TCP connect 成功 1 件は誰にも何も語りません）。
incident は段落であり、「これは前にも起きたか」を 1 年後に確認する対象です。

**削除されないもの（SQL で強制、設定で緩められません）:**

* **open な incident は年齢に関わらず削除されません。** 1 年開いている
  incident は 1 年直っていない障害であり、まさに残すべきものです
* **証拠は引用元より長生きします。** 生存している incident / diagnosis が
  参照している observation は、保持期間を過ぎていても残ります。
  証拠が消えた診断は誰も検証できない主張だからです（`SPEC.md` §116）
* **各 entity は直近の observation を必ず残します**（`keep_per_entity`）。
  これが無いと、保持期間より長く落ちている host は「見たことがある」証拠を
  すべて失います。最長の障害ほど消えるという逆転が起きます

`keep_per_entity` は診断が読む件数（32）を下回れません。
下回る値を設定した場合は 32 に引き上げられ、`config check` が警告します。

**容量の目安。** 実測で 5 host あたり約 10 KB/s、host 1 台あたり
1 日約 170 MB です。既定の 14 日保持なら host あたり約 2.4 GB で頭打ちになります。

prune は空きページを再利用可能にしますが、ファイルサイズは縮みません。
保持期間を下げた直後に領域を返したい場合は `sentinel prune --vacuum` を使います。

### `[tls]`

controller API の転送路保護です。**すべて任意で、追加的です。**
何も設定しなければ従来どおり平文 HTTP で動作します。

controller 側（listener）:

| キー | 型 | 意味 |
| --- | --- | --- |
| `cert` | path | server 証明書チェーン（PEM）。設定すると TLS が有効になる |
| `key` | path | server 秘密鍵（PEM: PKCS#8 / PKCS#1 / SEC1） |
| `client_ca` | path | client 証明書を検証する CA（PEM）。**設定すると client 証明書は必須になります** |

agent 側（client）:

| キー | 型 | 意味 |
| --- | --- | --- |
| `ca` | path | controller の証明書を検証する CA（PEM）。system root に**追加**されます |
| `client_cert` | path | controller に提示する client 証明書（PEM） |
| `client_key` | path | `client_cert` の秘密鍵（PEM） |
| `server_name` | 文字列 | 証明書の検証に使う名前。IP で接続する場合に使う |
| `insecure_skip_verify` | bool | 証明書を検証しない（既定 `false`） |

**3 つの構成:**

1. **何も設定しない** — 平文 HTTP。隔離された管理 network では今も正しい選択です
2. **`cert` + `key`** — TLS。agent は system root か `ca` で検証します
3. **`cert` + `key` + `client_ca`** — mutual TLS。agent は証明書を提示しなければ
   token を出すことすらできません。**token が漏れても耐えられる構成はこれだけです**

`client_ca` を設定した時点で client 証明書は「任意」ではなく「必須」です。
任意の client 認証はセキュリティのように読めて何も守りません
（攻撃者は提示しないだけです）。

`insecure_skip_verify = true` は TLS を装飾に変えます。接続を横取りできる
攻撃者は任意の証明書を提示でき、cluster credential はそのまま読まれます。
PKI より先に cluster が立ち上がる現実のために用意してありますが、
起動のたびに警告が出ます。

**証明書の自動生成機能はありません。** 監視システムが自前の trust anchor を
発行すれば、誰も監査しない private CA が 1 つ増えるだけです。
TLS を要求する現場には、既に証明書を発行する手段があります。

### `[peer_monitoring]`

| キー | 型 | 既定値 | 意味 |
| --- | --- | --- | --- |
| `degree` | 整数 | `3` | 1 entity あたりに割り当てる observer 数 |

`degree = 0` は peer monitoring を無効化し、警告されます。
第 2 の視点が無い場合、controller 自身の network path 上の障害と
host の死を区別できなくなるためです。

### `[discovery.slurm]`

| キー | 型 | 既定値 | 意味 |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Slurm discovery を実行するか |
| `scontrol_path` | path | *(`PATH` から解決)* | `scontrol` の場所 |

Slurm は複数ある inventory provider のうちの 1 つです。
Slurm 外の host も一級市民として扱われます。

### `[capabilities]`

Capability 名をキーとする運用者による上書き。

```toml
[capabilities]
"storage.nfs.server" = "force"    # discovery の結果によらず ON
"storage.smart"      = "disable"  # discovery の結果によらず OFF
"storage.zfs"        = "enable"   # discovery が何も言わなかった場合に ON
```

解決順序（強い順）:

```text
disable  >  force  >  runtime discovery  >  enable / role hint
```

### `[[entities]]`

まだ agent が入っていない、あるいはどの integration も発見しない entity を宣言します。

```toml
[[entities]]
type = "host"
name = "fileserver-a"
labels = { rack = "r01", location = "entrance" }
capabilities = ["storage.nfs.server", "observer.peer"]
addresses = ["10.0.0.10"]

[[entities]]
type = "storage"
name = "shared-a"
```

`type` は `host` / `service` / `storage` / `scheduler` / `external_dependency` のいずれか。

`name` は canonical name であり、`(environment, type)` 内で一意です。
address と port は到達性のためのデータであり identity ではありません
— 1 entity が複数 address を持てますし、port を変えても同じ entity です。

`ports` は **agent がいない host** に必要です。
agent がいる host は自分でポートを報告します。

```toml
[[entities]]
type = "host"
name = "fileserver-a"
addresses = ["192.0.2.10"]
ports = { ssh = 2222, agent = 9444 }
```

指定しない場合は既定値（ssh 22 / agent 7444 / nfs 2049）が使われます。
SSH を 22 以外で運用しているクラスタでこれを書き忘れると、
probe が閉じたポートを叩き、**全 host が SSH 障害として報告されます。**

### `[[dependencies]]`

`from` が `to` に依存します。両者とも `type/name` 形式で記述します。

```toml
[[dependencies]]
from = "host/compute-a"
to = "storage/shared-a"
type = "uses_storage"
criticality = "critical"

[[dependencies]]
from = "storage/shared-a"
to = "host/fileserver-a"
type = "provides"
```

`type` は `depends_on` / `hosted_on` / `provides` / `uses_storage` /
`uses_scheduler` / `network_reaches` / `observes`、
または integration 固有の任意文字列。

`criticality` は `critical` / `important` / `optional`。
`optional` の edge は障害を伝播しません。

ここで宣言していない entity を参照することは error ではなく warning です。
Slurm discovery や agent registration から正当に到着し得るためです。

cycle は許可されます。実際の依存グラフには cycle が存在します。

## 既定パス

| パス | 内容 |
| --- | --- |
| `/etc/sentinel/config.toml` | 設定 |
| `/var/lib/sentinel/` | database と spool |
| `/run/sentinel/` | runtime state |

ログは stderr へ出力されます。systemd 配下では journal に入ります。
