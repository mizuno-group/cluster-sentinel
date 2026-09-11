# コマンドリファレンス

`sentinel` はバイナリ 1 つです。controller も agent も調査用の CLI も、
すべてこのバイナリのサブコマンドとして提供されます。

---

## 早見表

**やりたいことから引く場合。**

| やりたいこと | コマンド |
| --- | --- |
| 今どうなっているか知りたい | `sentinel status` |
| 何が壊れていて、なぜそう言えるのか | `sentinel diagnose` |
| 対応が必要な障害の一覧 | `sentinel incident list` |
| その障害の根拠と経緯を全部見る | `sentinel incident show <id>` |
| **動いているはずの検査が動いているか** | `sentinel audit` |
| そもそもちゃんと監視できているのか | `sentinel explain paths` |
| 何をどんなコマンドで見張っているのか | `sentinel explain` |
| ある host の詳細を見る | `sentinel entity show <name>` |
| **その判断の元になった生データ** | `sentinel entity observations <name>` |
| 誰が誰を見張っているか | `sentinel peers` |
| 依存関係のグラフ | `sentinel dependency list` |
| このホストが Sentinel からどう見えるか | `sentinel doctor` |
| 通知が実際に届くか試す | `sentinel notify test` |
| 設定が正しいか確かめる | `sentinel config check` |
| 設定の実効値と、その出どころ | `sentinel config show` |
| 導入する（設定・unit・credential を生成） | `sentinel install controller` / `agent` |
| 手動で discovery を 1 周回す | `sentinel discover` |
| 古い記録を消す | `sentinel prune --dry-run` |

---

## 共通のこと

### 実行ユーザー

**`sentinel` ユーザーか root で実行してください。**

```bash
sudo -u sentinel sentinel status
```

設定ファイルは world-readable にしていません。webhook の URL 自体が
credential を含みうるためです。

### 全コマンド共通のオプション

| オプション | 意味 |
| --- | --- |
| `-c`, `--config <PATH>` | 設定ファイルの場所。既定 `/etc/sentinel/config.toml`。環境変数 `SENTINEL_CONFIG` でも指定可 |
| `--json` | 機械可読出力（`controller` / `agent` / `install` / `explain` を除く） |
| `-v`, `--verbose` | ログを詳しく。重ねるとさらに詳しく（`-vv`） |
| `--log-json` | ログを JSON で出す |

`--json` はほぼすべての調査系コマンドにあります。`jq` と組み合わせる前提の
安定した形なので、スクリプトからはこちらを使ってください。
（`explain` は人が読むためのものなので `--json` はありません。）

### 終了コード

**そのまま health check に使えます。**

| コマンド | `0` | `1` | `2` |
| --- | --- | --- | --- |
| `status` | 正常 | — | 異常あり（**open な incident を含む**） |
| `diagnose` | 何も問題なし | — | 診断結果あり |
| `incident list` | active な incident なし | — | active な incident あり |
| `config check` | 妥当 | error あり | — |
| `discover` | 全 provider 成功 | 失敗した provider あり | — |
| `notify test` | 全宛先に到達 | — | 失敗した宛先あり |
| `entity show` / `observations` | 成功 | 見つからない／曖昧 | — |
| `audit` | 沈黙している probe なし | — | 沈黙している probe あり |

---

## 状態を見る

### `sentinel status`

環境全体の一覧。**まずこれです。**

```bash
sentinel status
```

open な incident があれば、entity 一覧の**前に**表示されます。
その場合 exit code は 2 になります。

末尾には、必要なときだけ次の行が出ます。

- **agent のバージョンが混在している**とき（更新中は正常、更新後なら見落とし）
- **沈黙している probe があるとき**（`sentinel audit` へ誘導）

> **全部 HEALTHY を鵜呑みにしないでください。** この表示は、正しく監視
> できている状態と、そもそも何も見ていない状態で**同じ見た目**になります。
> 見分け方は `explain paths` と `entity observations` です
> （[OPERATIONS.md](OPERATIONS.md#全部-healthyを鵜呑みにしない)）。

### `sentinel diagnose`

**何が壊れていて、なぜそう言えるのか。** 診断結果とその根拠、
そして安全に実行できる調査コマンドの提案を出します。

```bash
sentinel diagnose
```

提案されるコマンドは**すべて read-only** です。Sentinel が自分で
`restart` や `scontrol update` を実行することはありません。

### `sentinel incident list`

障害として扱われているもの。通知が飛ぶのはこれです。

| オプション | 意味 |
| --- | --- |
| `--all` | 解決済みも含める（既定は active のみ） |

### `sentinel incident show <id>`

1 件を全部。timeline、根拠になった観測、疑わしい原因、影響範囲。

```bash
sentinel incident show 7f2c0ef6-eaee-485a-bb18-e6612411bfe9
```

id は `incident list` や通知本文からコピーできます。

---

## 監視の中身を見る

### `sentinel explain [capabilities|probes|paths]`

**`status` が「何を結論したか」を言うのに対し、これは「どうやって分かるのか」を
言います。** 引数を省略すると 3 つすべて出ます。

| 引数 | 内容 |
| --- | --- |
| `capabilities` | 何が検査を有効にするのか、どう判定されるのか |
| `probes` | 各検査が**実際に実行するコマンド**、間隔、タイムアウト |
| `paths` | どの host を誰が、何で監視しているか |

```bash
sentinel explain paths
```

出力の **`watched by`** が重要です。

```
compute02
  reached at   172.20.0.4
  from itself  host.metrics
  from others  network.tcp, sentinel.agent, ssh.service
  watched by   compute03 (SameDomain), filesrv02 (Independent), compute01 (Filler)
```

**`watched by` が空か 1 台しかない host は、実質的に監視されていません。**
到達性の判断には独立した視点が最低 2 つ必要で、1 台では判断を保留します。

### `sentinel audit`

**動いているはずの検査が、実際に動いているか。**

```bash
sentinel audit
```

`explain probes` は「何が動く**はず**か」を、`entity observations` は
「何が動い**た**か」を言います。**この 2 つを突き合わせるものが無かったため、
ある検査がどこからも実行されていないことに数か月気づけませんでした。**

検査が走らなければ観測が生まれず、観測が無ければ失敗もせず、失敗しなければ
診断もされません。**何も言われないので、すべて健全に見えます。**
沈黙している probe は、それ自体が異常です。

報告は 2 種類に分かれます。

| 種別 | 意味 |
| --- | --- |
| **never** | 一度も観測が無い。**どこからも実行されていない**可能性が高い |
| （時刻あり） | 以前は動いていたが止まった。agent の停止、capability の消失など |

probe ごとにまとめて表示されるので、**「適用される全ホストで沈黙」**が
一目で分かります。1 台だけ静かなのはそのホストの問題ですが、
**全ホストで静かなのは配線されていない probe** で、
これは他のどこにも現れません。

誤検知を避けるため、次は報告しません。

- 設定で明示的に無効化されている probe
- agent が居ないホストのローカル probe（実行する主体がいない）
- observer が付いていないホストのリモート probe（同上。`explain paths` の領分）
- 直近で 1 回取りこぼしただけのもの（**probe 間隔の 10 倍**、最低 5 分は待つ）

**沈黙があれば exit code 2** を返すので、cron や CI に置けます。

```bash
sudo -u sentinel sentinel audit > /dev/null || echo "監視に穴があります"
```

`status` の末尾にも 1 行だけ出ます。**この検査自体を実行し忘れると同じことに
なる**ので、聞かれなくても言うようにしてあります。

```
⚠ 1 probe(s) have never reported at all; run `sentinel audit` for which
```

### `sentinel entity list`

| オプション | 意味 |
| --- | --- |
| `--type <TYPE>` | `host` / `service` / `storage` / `scheduler` などで絞る |

### `sentinel entity show <name>`

1 つの entity の詳細。capability、component ごとの健全性、
どのアドレスで probe されているか、**agent のバージョン**、報告されたハードウェア。

```bash
sentinel entity show compute02
```

**名前が重複する場合は `type/name` で指定します。** storage entity は
提供元ホストと同じ名前を名乗るためです。

```bash
sentinel entity show host/filesrv02      # ホストそのもの
sentinel entity show storage/filesrv02   # そのホストが提供するストレージ
```

曖昧なまま指定した場合、勝手にどちらかを選ばずに候補を表示します。
entity id をそのまま渡すこともできます。

### `sentinel entity observations <name>`

**判断の元になった生の観測。** いつ・誰が・何を見たか。

```bash
sentinel entity observations compute02
```

| オプション | 意味 |
| --- | --- |
| `--probe <ID>` | 特定の検査だけ（例 `network.tcp`） |
| `--limit <N>` | 件数。既定 40 |

`--probe` を付けるとその検査だけを絞って検索します。**「観測がありません」と
出たら、その検査は本当に走っていません**（他の検査に埋もれて見えないのでは
ありません）。capability が付いていない可能性が高いので、
`entity show` を確認してください。

### `sentinel peers`

observer の割り当て。observer が付いていない entity は明示されます。

### `sentinel dependency list`

依存関係の一覧。NFS のマウント関係は agent の報告から自動で導出されるので、
**設定ファイルに書いた覚えがなくてもここに出てきます。**

### `sentinel doctor`

**そのホスト自身**が Sentinel からどう見えるか。capability の判定理由、
報告するアドレスとその選定理由、spool の深さ。

```bash
sentinel doctor
```

「なぜこの検査が動かないのか」を調べるときは、対象ホストで
これを実行するのがいちばん速いです。

---

## 設定

### `sentinel config check`

**起動前に必ず。** ここで落ちるなら起動しても落ちます。

```bash
sudo -u sentinel sentinel config check
```

error と warning を区別します。「宣言していない entity を参照している」は
warning です — Slurm discovery や agent の登録から正当に到着しうるためです。

### `sentinel config show`

**実効値と、その値がどこから来たのか。** 既定値なのか設定ファイルなのかが
分かるので、「設定したはずなのに効いていない」の調査に使います。

### `sentinel config init`

全項目を既定値つきで書き出します。`install` が内部で使っているものと同じです。

| オプション | 意味 |
| --- | --- |
| `--role <ROLE>` | `controller` または `agent`（既定 `agent`） |
| `--output <PATH>` | 出力先。省略時は設定パス |
| `--dry-run` | 書かずに表示する |
| `--force` | 既存ファイルを上書き |

---

## 導入・保守

### `sentinel install <controller|agent>`

設定ファイル・systemd unit・（controller なら）cluster credential を生成します。
**手で書くファイルはありません。**

```bash
sudo sentinel install controller
```

| オプション | 意味 |
| --- | --- |
| `--config <PATH>` | 設定ファイルの場所。**同居させるときに使う**（下記） |
| `--binary <PATH>` | unit が実行するバイナリのパス |
| `--output-dir <DIR>` | unit の出力先。既定 `/etc/systemd/system` |
| `--dry-run` | 何も書かずに全部表示する |
| `--force` | 既存ファイルを上書き（**credential は決して上書きしません**） |
| `--no-credential` | controller でも credential を作らない |

実行後に「次にやること」が表示されます。**すでに済んでいる手順は
表示されません。**

既存ファイルは `--force` なしには触りません。credential だけは `--force` を
付けても上書きされません — 入れ替えるとクラスタ中の agent が締め出されるためです。

**controller と agent を同じホストで動かす場合**は設定パスを分けます。
既定のままだと controller の設定が上書きされます。

```bash
sudo sentinel install agent --config /etc/sentinel/agent.toml
```

詳細は [DEPLOYMENT.md §9.10](DEPLOYMENT.md#910-controller-に-agent-を同居させる)。

### `sentinel discover`

inventory の discovery を 1 周だけ手動で回します。controller は自動で
回しているので、通常は不要です。設定を変えた直後の確認に使います。

NFS のマウントで**解決できなかったアドレス**があれば、ここで報告されます。

### `sentinel prune`

保持期間を過ぎた記録を削除します。controller が自動で行うので通常は不要です。

| オプション | 意味 |
| --- | --- |
| `--dry-run` | **削除せずに量だけ報告** |
| `--observations <PERIOD>` | この実行に限り観測の保持期間を上書き |
| `--vacuum` | 空き領域をファイルシステムへ返す。database 全体を書き直すので任意 |

```bash
sudo -u sentinel sentinel prune --dry-run
```

保持期間を短くする前に `--dry-run` で影響を確かめられます。
**open な incident と、生きている incident が参照している観測は
設定に関わらず削除されません。**

---

## 通知

### `sentinel notify test`

設定した宛先に、テスト通知を送ります。**障害を待つ必要はありません。**

```bash
sudo -u sentinel sentinel notify test
```

| オプション | 意味 |
| --- | --- |
| `--provider <NAME>` | その宛先だけ |
| `--severity <LEVEL>` | 送る severity。既定 `warning` |

**宛先ごとに 1 通だけ**送ります。incident も database も重複排除も
一切触りません。`min_severity` の下でも送られます — これは「起こす価値が
あるか」の設定であって、疎通確認とは別の話なので。

URL の打ち間違いを障害の最中に知るのが最悪なので、設定したら必ず
一度実行してください。

---

## デーモン

### `sentinel controller` / `sentinel agent`

デーモン本体。通常は systemd から起動されるので、手で叩くのは
**デバッグのときだけ**です。

```bash
sudo -u sentinel sentinel -vv controller
```

`-v` を重ねると各サイクルの詳細が出ます。

---

## `sentinel version`

```
sentinel 1.0.0
protocol version: 1
config version:   1
target:           x86_64-unknown-linux-musl
```

**3 つのバージョンは別々に動きます。**

- **binary version** — このバイナリのリリース
- **protocol version** — controller と agent の通信規約。これが一致していれば、
  binary version が混在していても動きます（アップグレード中はそうなります）
- **config version** — 設定ファイルの形式。未知の値は起動時に拒否されます

---

## よく使う組み合わせ

**障害の報告を受けたとき**

```bash
sentinel status && sentinel diagnose && sentinel incident list
```

**特定のホストがおかしいとき**

```bash
sentinel entity show <host> && sentinel entity observations <host>
```

**導入直後・構成変更後の確認**

```bash
sentinel explain paths && sentinel dependency list
```

**監視スクリプトから**

```bash
sentinel status --json | jq -r '.entities[] | select(.health != "healthy") | "\(.entity_type)/\(.name) \(.health)"'
```

**cron から健全性を見る**（異常なら exit 2）

```bash
sudo -u sentinel sentinel status > /dev/null || echo "cluster-sentinel: 異常あり"
```
