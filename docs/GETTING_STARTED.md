# はじめての Cluster Sentinel

**はじめて触る人のための案内です。** ここを最後まで読むと、クラスタの状態が
`sentinel status` で見えるようになります。所要 30 分ほど。

細かい設定項目は出てきません。まず動かして、動いているものを見ながら
覚えるほうが早いためです。詳しい話は最後にリンクがあります。

---

## 1. これは何をするものか

「ノードが応答しない」と言うだけの監視は、たいてい役に立ちません。
本当に知りたいのは **なぜ応答しないのか** で、原因によって取るべき行動が
まったく違うからです。

Sentinel が区別しようとしているのは、たとえばこういう違いです。

| 見た目 | 実際に起きていること | やること |
| --- | --- | --- |
| node に繋がらない | **本当に落ちている** | 現地を見に行く |
| node に繋がらない | **経路の一部だけが切れている** | ネットワークを見る |
| node に繋がらない | **SSH だけ死んでいる**（マシンは生きている） | sshd を直す |
| node が使えない | **Slurm 上で drain されているだけ** | 誰が drain したか調べる |
| 何台も同時に不調 | **共有ストレージ 1 台が原因** | その 1 台を見る |

どれも「応答しない」に見えますが、行き先が全部違います。
**間違ったマシンに人を送らないこと** が、このツールの目的です。

そのために Sentinel は、1 か所からではなく **複数のノードから互いを観測** し、
それらの証言を突き合わせて結論を出します。1 台からしか見えていない不調を
「ホストが落ちた」と言い切ることはしません。

---

## 2. 最低限の用語

読み進めるのに必要なのはこれだけです。

**controller**
: 全体を束ねる 1 台。観測結果を集め、判断し、通知します。
  ふつうはヘッドノードに置きます。

**agent**
: 各ノードで動く常駐プロセス。自分自身のことを報告し、
  ついでに**他のノードを見張ります**（この見張り合いが上の「複数の視点」です）。

**entity（エンティティ）**
: 監視対象 1 つ 1 つ。ホスト、サービス、ストレージなど。
  「ホスト filesrv02」と「filesrv02 が提供するストレージ」は**別のもの**として
  扱います。マシンは生きているが export だけ死んだ、を言えるようにするためです。

**capability（ケーパビリティ）**
: そのノードが「何を持っているか」。GPU がある、NFS を使っている、など。
  **役割名ではなく実際に見つかったもの**で決まり、これによって
  どの検査を動かすかが自動的に決まります。
  「このノードは計算ノードだから GPU 検査を回す」ではなく
  「`nvidia-smi` があるから回す」という考え方です。

**probe（プローブ）**
: 実際の検査 1 つ 1 つ。TCP で繋いでみる、`systemctl` で状態を見る、など。

**incident（インシデント）**
: 「これは障害だ」と判断されたもの。通知が飛ぶのはこれです。

---

## 3. 準備するもの

- controller にする 1 台（ヘッドノードで構いません）
- agent を入れるノード（**3 台以上を推奨**。理由は後述）
- 全ノードから controller の **TCP 7443** に届くこと
- ノード同士が **TCP 7444** で届くこと（見張り合いに使います）

> **なぜ 3 台以上か**
> 「A から B が見えない」だけでは、B が落ちているのか A と B の間が
> 切れているのか分かりません。複数のノードが同じことを言って初めて
> 「B が落ちた」と言えます。1 台だと Sentinel は判断を保留します。
> それは仕様であって、不具合ではありません。

コンパイルは不要です。バイナリ 1 つで controller も agent も CLI も兼ねます。

```bash
ARCH=$(uname -m) && curl -fsSL -o sentinel "https://github.com/mizuno-group/cluster-sentinel/releases/latest/download/sentinel-${ARCH}-unknown-linux-musl" && chmod +x sentinel && sudo mv sentinel /usr/local/bin/
```

x86_64 と ARM が混在していても、各ノードで上を実行すれば正しいものが入ります。

```bash
sentinel version
```

---

## 4. controller を建てる

### 4.1 一式を生成する

```bash
sudo sentinel install controller
```

これだけで、設定ファイル・systemd unit・クラスタ credential が作られます。
**手で書くファイルはありません。** 設定ファイルには全項目が既定値つきで
書き出され、変えるべき行にだけ `CHANGE-ME` が入っています。

コマンドの最後に「次にやること」が表示されます。**その通りに進めれば済みます**
（すでに済んでいる手順は表示されません）。以下はその補足です。

### 4.2 サービスユーザーを作る

Sentinel は root では動きません。読むだけの常駐プロセスなので、
専用の非特権ユーザーで動かします。

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel
```

```bash
sudo install -d -o sentinel -g sentinel -m 0750 /var/lib/sentinel && sudo chown -R sentinel:sentinel /etc/sentinel
```

```bash
sudo usermod -aG systemd-journal sentinel
```

最後の 1 行は、カーネルログを読めるようにするためのものです。
入れておかないと、ディスク I/O エラーのような**いちばん知りたい種類の証拠**を
拾えません。

### 4.3 環境名を決める

設定ファイルの `CHANGE-ME` を書き換えます。環境名は**全ノードで一致**させる
必要があります。クラスタの名前でも研究室名でも構いません。

```bash
sudo sed -i 's/^environment = .*/environment = "my-cluster"/' /etc/sentinel/config.toml
```

Slurm を使っているなら、次を有効にします（`scontrol` から自動でノード一覧を
取ってくるので、ノードを手で列挙する必要がなくなります）。

```toml
[discovery.slurm]
enabled = true
```

書けたら確認します。**ここで落ちるなら、起動しても落ちます。**

```bash
sudo -u sentinel sentinel config check
```

### 4.4 起動する

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now sentinel-controller
```

```bash
sudo -u sentinel sentinel discover && sudo -u sentinel sentinel status
```

Slurm を有効にしたなら、この時点でノードが一覧に出ます。まだ agent が
いないので、多くが `UNKNOWN` のはずです。それで正常です。

---

## 5. agent を配る

### 5.1 credential を配る

全ノードが**同じ credential** を持つ必要があります。controller が作ったものを
そのまま配ります。

```bash
sudo scp /etc/sentinel/token <node>:/etc/sentinel/token
```

> **上書きに注意。** すでに動いているクラスタで別の credential を置くと、
> **全 agent が締め出されます。** 配るのは初回だけです。

### 5.2 各ノードで

```bash
sudo sentinel install agent
```

controller と同じように、設定と unit が生成されます。書き換えるのは 2 行だけです。

```bash
sudo sed -i 's/^environment = .*/environment = "my-cluster"/; s/^controller_address = .*/controller_address = "head:7443"/' /etc/sentinel/config.toml
```

サービスユーザーの作成（4.2）を各ノードでも行ってから、起動します。

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now sentinel-agent
```

### 5.3 台数が多い場合

手で回るのは現実的ではないので、Ansible ロールを同梱しています。
**controller の設定を読み取って、揃えるべき項目を自動で配ります**
（環境名や監視頻度を 2 か所で管理しなくて済みます）。

[`deploy/ansible/`](../deploy/ansible/) を参照してください。

> **controller のホストを `agents` グループに入れないでください。**
> ロールは決まったパスに設定ファイルを書くため、controller の設定が
> 上書きされて controller が止まります。controller に agent を同居させる
> 方法は [DEPLOYMENT.md](DEPLOYMENT.md) にあります。

---

## 6. 動いていることを確かめる

```bash
sudo -u sentinel sentinel status
```

しばらく待つと `UNKNOWN` が `HEALTHY` に変わっていきます。

**ここで満足しないでください。** 全部 HEALTHY という表示は、
正しく監視できている状態と、**そもそも何も見ていない状態**の
両方で同じに見えます。次の 2 つで中身を確認します。

```bash
sudo -u sentinel sentinel explain
```

何が検査を有効にしているか、各検査が実際にどんなコマンドを実行するか、
どのノードが誰を見張っているかが出ます。**「見張り役が 0 台」のノードが
あれば、そこは実質的に監視されていません。**

```bash
sudo -u sentinel sentinel entity observations <node-name>
```

そのノードについて、いつ・誰が・何を観測したかの生データです。
時刻が現在時刻の近くで更新され続けていれば、本当に見えています。

そして、これを**毎回手で確かめなくて済むように**するのが次のコマンドです。

```bash
sudo -u sentinel sentinel audit
```

「有効なのに観測を出していない検査」を挙げます。何も無ければ 1 行で終わります。
**cron に置いておくのを勧めます**（穴があれば exit code 2 を返します）。

---

## 7. 通知を設定する

設定しないと、障害が起きても `status` を見に行くまで気づけません。

controller の設定ファイルに追記します。

```toml
[notification]
min_severity = "warning"

[[notification.webhooks]]
name   = "ops"
url    = "https://hooks.slack.com/services/XXX/YYY/ZZZ"
format = "slack"
```

`format = "slack"` にすると、色つきの帯と太字で整形されます。
それ以外の宛先なら `"generic"` のままにしてください。

障害を待たずに、届くかどうかだけ先に試せます。

```bash
sudo -u sentinel sentinel notify test
```

**宛先ごとに 1 通だけ**送られます。URL の打ち間違いを障害の最中に
知るのが最悪なので、先に確認しておいてください。

通知は**状態が変わったときだけ**飛びます。続いている障害を
繰り返し通知することはありません。復旧時にも届きます。

---

## 8. 最初につまずきやすいところ

**SSH が 22 番ではない**
: agent が `/etc/ssh/sshd_config` を読んで自動的に検出します。何もしなくて
  構いません。agent を入れていないホストだけ、設定ファイルで教える必要が
  あります（[DEPLOYMENT.md](DEPLOYMENT.md) の該当節）。

**`config check` が通らない**
: メッセージがどの行の何が問題かを言います。`CHANGE-ME` の消し忘れが
  いちばん多いです。

**agent が起動直後に落ち続ける**
: 設定ファイルを `sentinel` ユーザーが読めていない可能性があります。
  `sudo chown -R sentinel:sentinel /etc/sentinel` を実行してください。

**ストレージの依存関係を書かないといけない?**
: **不要です。** agent が報告するマウント表から自動的に組み立てられます。
  ノードがマウント先を変えても設定ファイルを触る必要はありません。

**あるノードだけ `UNKNOWN` のまま**
: そのノードの agent が登録できていません。ノード側で
  `systemctl status sentinel-agent` と `journalctl -u sentinel-agent -n 50` を
  見てください。credential の不一致か、controller への到達性が大半です。

---

## 9. 次に読むもの

| 目的 | ドキュメント |
| --- | --- |
| **コマンドの一覧と使い分け** | [COMMANDS.md](COMMANDS.md) |
| 本番クラスタへ本格導入する（TLS、非標準ポート、段階導入） | [DEPLOYMENT.md](DEPLOYMENT.md) |
| 日々の運用と、障害が出たときの読み方 | [OPERATIONS.md](OPERATIONS.md) |
| 設定項目を全部知りたい | [CONFIGURATION.md](CONFIGURATION.md) |
| 何をどこまで守るツールなのか | [SECURITY.md](SECURITY.md) |
| 設計の考え方 | [ARCHITECTURE.md](ARCHITECTURE.md) |
