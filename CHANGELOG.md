# 変更履歴

本プロジェクトは milestone 単位で構築されています
（[docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md) §94）。

## v0.3.1

実機導入で見つかった不具合の修正と、まっさらな host からの導入手順。

* **到達性 probe が capability を要求しなくなった。** Slurm discovery で
  見つかった host には 1 つも probe が走っておらず、
  誰も接触していない host が HEALTHY と表示されていた
* **実クラスタの識別子をドキュメントから削除。**
  ホスト名・VLAN・IP・実 MAC 由来の link-local アドレス
* 報告アドレスの選択（loopback interface 上のアドレスを除外、
  複数 NIC の曖昧さを報告、`[agent] interface` / `address`）
* systemd unit の `StateDirectory=`、`install --binary`、
  ダウンロード先から実行した場合の警告
* `sentinel install` が `scontrol` を検出して Slurm discovery を設定
* Ansible ロール（`deploy/ansible/`）
* release workflow（x86_64 / aarch64 の静的リンクバイナリ）

## v0.3.11

* **`[[notification.webhooks]] format`**（新規）。宛先ごとに payload の形を選ぶ。
  * `slack` — Block Kit。色つきの帯・見出し・太字・整形済みの詳細。
    **復旧は重大度に関わらず緑**（色が最初に伝えるべきなのは
    「始まったのか終わったのか」であるため）。
    見出しには重大度だけを置き、要約からは接頭辞を外す
    （色がすでに言っていることを繰り返すと 1 行目を浪費する）。
  * `generic`（既定）— 従来どおり。既存の宛先の挙動は変わらない。
  * Slack は長すぎる `header` を切り詰めず**拒否する**ため、
    全ブロックを上限内に収めている。

## v0.3.10

* **Slack への通知が 400 で弾かれていた**（バグ修正）。
  Slack の incoming webhook は `text`（または `blocks` / `attachments`）を
  要求し、無ければ `missing_text_or_fallback_or_attachments` を返す。
  汎用 JSON をそのまま送っていたため、**Slack 宛の通知は 1 件も届かなかった。**
  * payload に `text`（Slack / Microsoft Teams）と
    `content`（Discord）を追加した。中身は title・body・推奨アクションを
    まとめた読める文章。
  * 構造化されたフィールドはそのまま残っているので、
    severity で振り分ける受け手は影響を受けない。
  * 各サービス専用の provider を書けば色やスレッドも使えるが、
    **前段に変換を挟まないと動かない webhook は、多くの人にとって
    動かない webhook**なので、URL だけで動くことを優先した。

## v0.3.9

* **incident が開いても通知されないことがあった**（バグ修正）。
  correlation が discovery ループと診断ループの**両方**で走っており、
  通知を送るのは後者だけだった。先に走ったほうが「開いた」という事実を
  消費するため、**discovery cycle で開かれた incident は記録されるだけで
  一度も通知されない。**
  * 非決定的に起きる通知漏れであり、**動いているように見えるぶん
    通知が無いより悪い。**
  * incident を開く場所を 1 箇所（診断ループ）に統一した。
    discovery は inventory・observation・state までを担い、
    診断結果は報告するが incident は作らない。
  * `DiscoveryReport` から `incidents_opened` / `incidents_resolved` を削除。

## v0.3.8

* **`sentinel notify test`**（新規）。設定した通知先に届くかを、
  障害を待たずに確かめられる。宛先ごとに成否を表示する。
  incident も database も重複排除も触らない。
  URL の打ち間違いを障害の最中に知るのが最悪なので、
  その前に答えられる必要がある。
  * 送る内容は、人が見てもフィルタが見てもテストと分かるようにしてある。
  * `fingerprint` は固定値で、実際の incident と衝突しない
    （衝突すれば本物の通知を黙らせてしまう）。
  * `min_severity` の下でも送る。floor は「起こす価値があるか」を
    決めるものであり、確かめているのは到達できるかだけ。
* `docs/OPERATIONS.md` に、通知経路の確認と、
  本番で試せる最小の実障害（agent の停止）を追加。

## v0.3.7

* **service entity を誰も観測していなかった**（バグ修正）。
  `slurmd@<node>` は inventory に存在するのに、probe が 1 つも向いておらず、
  実クラスタで **31 entity 中 13 が永久に UNKNOWN** だった。
  service を独立した entity にしているのは
  「デーモンが死んだ」と「マシンが死んだ」を別の答えにするためであり、
  service を誰も測らなければその区別は存在できない。
  * unit を systemd に訊くのは local な問い（controller は肩代わりできない）。
    agent が、自分の capability が示す unit を監視するようにした
    （`slurm.compute` → `slurmd`、`slurm.controller` → `slurmctld`）。
  * **観測は service に帰属させ、host には帰属させない。**
    host に混ぜるとデーモンとマシンの区別が消える。
  * capability による判定であり role では決めない。capability の判定には
    設定とバイナリの両方が必要（`docs/adr/0003`）。
  * systemd の無い host には**何もスケジュールしない**
    （UNSUPPORTED を出し続けるより無いほうがよい）。

## v0.3.6

* **agent が自分の sandbox の mount table を読んでいた**（バグ修正）。
  生成される systemd unit は `ProtectSystem=strict` を設定するため、
  サービスは**ファイルシステム全体が read-only に再マウントされた
  専用の mount namespace**で動く。probe が読んでいた `/proc/self/mounts` は
  その内側から見た姿であり、**書き込み可能な NFS 共有が全ノードで
  read-only と報告されていた。** 自分の sandbox を説明して、
  それをホストの状態と称していたことになる。
  ホストの mount table（`/proc/1/mounts`）を読むようにした。

## v0.3.5

* **到達性 probe が SSH ポートを無視して 22 番に固定されていた**（バグ修正）。
  SSH を 22 以外に移し、22 番を firewall で DROP している環境では、
  **健全な host が到達不能と報告される。** 実クラスタで 13 台が該当した。
  22 番が REJECT を返す host だけが「到達可能」と判定され、
  同じクラスタ内で結果が割れていた。
  probe は `ports.ssh`（agent が `sshd_config` から自動検出して報告する値）を
  優先して叩くようになった。refusal を成功とみなす点は変わらない。
* **`sentinel` ユーザーが `systemd-journal` group に入っていなかった。**
  journal が読めるホストでも `journal.events` が UNSUPPORTED になり、
  kernel event が一切収集されていなかった。
  `install` の案内と Ansible ロールの両方で group に追加する。
* **read-only な NFS mount を「劣化」と判定しなくなった**（誤検知の修正）。
  `/proc/mounts` の `ro` は「読み取り専用である」ことしか示さず、
  「読み取り専用に落ちた」かどうかは分からない。意図的に ro で
  export / mount している共有は珍しくなく、それを常時 DEGRADED と
  報告するのは恒久的な誤警報で、storage component 全体が信用されなくなる。
  事実として記録するだけにした。**本当に kernel が ro に落とした場合は
  journal probe（`filesystem_readonly`）が捉える。**
* `docs/DEPLOYMENT.md` に firewall で開けるポートの節を追加。

## v0.3.4

Ansible ロールのみの変更です。バイナリに変更はありません。

* **controller の設定が唯一の出所になった。** ロールが実行時に controller の
  `config.toml` を読み、揃っていなければならない設定
  （`environment` / `[probes]` / `[tls]` の client 側 / 待ち受けポート）を
  各ノードへ配る。読み取りは controller 自身のバイナリ
  （`config show --json`）で行うため、daemon が解釈するのと同じ値が配られる。
  監視頻度の変更が controller 1 箇所の編集で済む。
* **credential が 0750 になっていた**（バグ修正）。所有者とモードを
  `recurse` で一括設定していたため、再帰的な `file` タスクが
  ディレクトリ用のモードをファイルにも適用し、直前に 0400 で書いた
  token を group 読み取り可能かつ実行可能にしていた。
* **毎回バイナリを再ダウンロードしていた**（バグ修正）。
  `sentinel_version` は `v0.3.3`、`sentinel version` の出力は `sentinel 0.3.3`。
  先頭の `v` のせいで比較が一致せず、実行のたびに全ノードが取得していた。
* **`-e sentinel_observer=false` が observer を有効にしていた**（バグ修正）。
  `-e` で渡された値は文字列で、`"false"` は真。boolean を全て `| bool` で受ける。
* **controller に対して実行すると controller の設定を破壊していた。**
  `sentinel-controller.service` があるホストでは実行を拒否する。
  `sentinel_manage_config=false` でバイナリと unit のみの配布も可能。
* `[probes]` を inventory から設定できるようになった（同期を使わない場合）。
* 冪等性を検証: まっさらから 1 回目 `changed=9`、2 回目以降 `changed=0`。

## v0.3.3

実機での切り分けに必要だったものと、Ansible ロールの修正。

* **`sentinel entity observations <name>`**（新規）。
  state や diagnosis ではなく、**どの観測者が何を見たか**を直接表示する。
  時刻 / probe / observer / status / アドレスと失敗理由。
  「SSH は通るのに到達不能」のような一見矛盾した状態は、
  観測者ごとの食い違いであることが多く、observer 列を見れば矛盾でなくなる。
* **`sentinel entity show` が probe 先アドレスと、その決まり方を表示する。**
  agent が報告したアドレスなのか、entity 名から毎回解決しているのかは、
  probe の成否を左右するが、これまで出力に無かった。
* **`doctor` の credential 判定が環境変数しか見ていなかった。**
  daemon は unit の `SENTINEL_TOKEN_FILE` から読むため、対話シェルでは
  常に NOT CONFIGURED と表示されていた。設定ファイルの隣の token も見る。
* **Ansible ロールの `sentinel_version` が v0.3.0 のままだった。**
  ロール自身が使う `install --binary` を持たないバイナリを配っていた。
  リポジトリの版と一致していないとテストが落ちるようにした。
* Ansible ロールがアーキテクチャ別の成果物を明示的な対応表で選ぶ。
* Ansible: SSH ユーザーの決まり方、ノードごとに異なる sudo パスワード
  （暗号化ファイル 1 つで済む方法）を文書化。

## v0.3.2

* **`install --force` が credential を上書きしなくなった**（バグ修正）。
  unit を更新するために `--force` を実行すると、cluster credential が
  再生成され、**全 agent が一斉に締め出されていた。**
  「ファイルを書き直す」という意味の flag が巻き込んでよい対象ではない。
  意図的な更新は、ファイルを削除してから `install` を実行する。
* アップグレード手順を `docs/OPERATIONS.md` に具体化
  （バイナリ入れ替え、順序、unit 更新時、切り戻し）。
* Ansible ロールが SSH / sudo のパスワード認証環境で動くように。

M0-M10（core scope）完了。
CLI からクラスタ状態と障害原因を説明できる状態です。

`IMPLEMENTATION.md` §97 の v1 受け入れ手順を
`dev/compose/scripts/acceptance` として自動化しており、
Docker 疑似クラスタに対して 23 項目すべてが通ります。

### 横断的な事項

* **到達性 probe が capability を要求しなくなった**（バグ修正）。
  実機導入で発覚。Slurm discovery で見つかった host は
  `slurm.compute` しか持たないため、`network.tcp` を要求していた
  reachability probe が **1 つも動いていなかった**。
  結果として、**誰も接触していない host が HEALTHY と表示されていた**
  （Slurm が IDLE と言っているだけ）。
  * TCP 接続を開くのに相手側に必要なものは何も無い。必要なのはアドレスだけで、
    それは呼び出し側が既に持っている。capability が門番をすべきなのは
    `nvidia-smi` や `journalctl`、NFS export のように
    **対象に何かが存在すること**を要求する probe である。
  * これで agent の無い host も、controller と peer から実際に確認される。
* **controller は自分自身の host を観測しない**（peer 割り当てと同じ理由）。
  controller が「自分の host は応答する」と報告しても何も証明していない
  （応答していなければ報告できない）。UNKNOWN のままにしておくほうが
  「誰も独立に見ていない」という事実を正しく表し、
  対処（そこに agent を置く / peer observer を付ける）を促す。
* **Ansible ロール**（`deploy/ansible/`）。
  ノードが 10 台を超えると手作業は現実的でない。
  アーキテクチャ別のバイナリ取得・チェックサム検証・`sentinel install`・
  credential 配布・`config check`・起動まで。
  Sentinel 側に Ansible 固有のものは無い。
* **報告アドレスの選択**（バグ修正 + 新規機能）。
  agent が報告するアドレスは peer が最初に叩く先であり、
  間違えると健全な host が到達不能に見える。
  診断対象そのものを壊すため、影響が大きい。
  * **バグ:** loopback 判定が *アドレス* だけを見ており、
    *インターフェース* を見ていなかった。`lo` に付いた非 loopback アドレス
    （WSL の `10.255.255.254/32` など）が残り、文字列順で先頭に来ていた。
    開発機で実際に再現。誰からも到達できないアドレスを報告していた。
  * **バグ:** アドレスの文字列ソートで先頭を選んでいたため、
    container bridge が実 NIC を追い越しうる。
  * 到達不能なもの（loopback、link-local、`lo` 上の全アドレス）を除外し、
    物理 NIC を仮想 NIC より優先、IPv4 を IPv6 より優先する順位付けに変更。
  * **自動検出では答えられない問いがあることを明示した。**
    `vlan101` / `vlan102` / `vlan103` を持つ host で、
    どれがクラスタ内通信を担うかは site の事実であり host の性質ではない。
    物理 NIC 候補が複数ある場合は **曖昧であると報告する**。
    黙って選ぶと、間違っていても気づけない。
  * `[agent] interface`（NIC 名）と `[agent] address`（直接指定）を追加。
    `interface` の指定先にアドレスが無い場合、**別の NIC にフォールバックせず
    何も報告しない。** 運用者が選ばなかったネットワークに peer を
    向けるのが、この設定で防ぎたい障害そのものだから。
  * `sentinel doctor` が、報告されるアドレス・候補一覧・
    曖昧な場合の警告を表示する。
* **監視頻度の設定**（新規機能、`[probes]`）。
  コンパイル時の既定値は数百ノード・健全な network を想定したもので、
  どこでも正しいわけではない。変更できなければ、
  誤った頻度で動かすか動かさないかの二択になる。
  * probe id ごとに `interval` / `timeout` / `max_outstanding` / `enabled`。
  * 書かれていない probe は既定のまま。1 つ調整しても他に影響しない。
  * override は `ProbeDefinition` 自体に適用する。decorator で包まないのは、
    いくつかの probe が自分の timeout で実行コマンドを制限しているため。
    runner だけが知る timeout は「同じ名前の別の値」になる。
  * **`max_outstanding` は引き下げのみ。** `nfs.client.io` と
    `journal.events` の同時実行 1 は、blocking syscall を積み上げないための
    制約であり（`SPEC.md` §76）、設定ファイルで覆せない。
  * 存在しない probe id は **error**。黙って無視されると
    「変更したつもりで変わっていない」状態になる。
  * controller の remote probe と agent の peer probe にも同じ override が効く。
    観測者ごとに頻度が違うと quorum が異なる頻度の観測を比較することになる。
  * `src/probes/catalog.rs` を probe 一覧の唯一の出所として追加。
* **設定ファイルの自動生成**（新規機能）。
  バイナリを持ち込んだ最初の 5 分が転記作業になっていた。
  手順を印刷して人間に実行させると、飛ばされるのは必ず credential の行。
  * `sentinel install <role>` が設定ファイル・systemd unit・
    （controller のみ）cluster credential をまとめて生成する。
  * 設定ファイルには **全設定が既定値のまま**、説明つきで書き出される。
    値は `Config::default()` と probe catalog から生成するため、
    バイナリが持っていない既定値をファイルが主張することはない。
  * 書き換えが必要な行だけ `CHANGE-ME` が入る
    （controller は 1 行、agent は 2 行）。
  * **既存ファイルは上書きしない。** 二度実行しても安全で、
    credential が入れ替わって全 agent が締め出されることもない。
    `--force` を付けた場合のみ上書きする。
  * credential は 32 byte 乱数、mode 0400。
    agent には生成しない（クラスタの誰も知らない credential ができるため）。
  * token の位置は `--config` の隣に決まる。unit の
    `SENTINEL_TOKEN_FILE` も同じ場所を指す。
  * `sentinel config init [--role R] [--output P] [--dry-run] [--force]`。
* **記録の保持期間**（新規機能、`[retention]`）。
  database は書き込み一方で、削除する経路がコードのどこにも無かった。
  実測で 5 host あたり約 10 KB/s、host 1 台あたり 1 日約 170 MB。
  100 ノードなら 1 日 17 GB で、放置すればディスクを埋めて controller ごと死ぬ。
  * class ごとに期間を設定する（observation 14d / transition 90d /
    解決済み incident 180d / 孤立 diagnosis 30d が既定）。
    バイト単価あたりの価値が違うため、単一の期間では表現できない。
  * `"never"` で個別に無期限保持を選べる。
  * **open な incident は年齢に関わらず削除しない。**
    1 年開いている incident は 1 年直っていない障害である。
  * **証拠は引用元より長生きする。** 生存している incident / diagnosis が
    参照する observation は保持期間を過ぎても残る。
    証拠が消えた診断は誰も検証できない主張になる（`SPEC.md` §116）。
  * **各 entity は直近 `keep_per_entity` 件を必ず残す。**
    これが無いと、保持期間より長く落ちている host が
    「見たことがある」証拠をすべて失う。最長の障害ほど消えるという逆転になる。
  * controller 内で 1 時間ごと、および起動時に実行。
  * `sentinel prune [--dry-run] [--vacuum] [--observations <期間>]`。
    `--dry-run` の件数は実際の DELETE を実行して rollback したもので、
    別の COUNT クエリではない（本番と食い違わないため）。
* **TLS**（新規機能、`[tls]`）。
  credential は bearer token であり、wire を読める者は全 agent に
  なりすませる。従来の答えは「reverse proxy を置け」で、
  これはシステムの性質ではなく運用者への宿題だった。
  * `cert` + `key` で controller が TLS listen する。
  * `client_ca` を書くと client 証明書が **必須** になる（任意にはならない。
    任意の client 認証は攻撃者が提示しないだけで無効化される）。
    **token が漏れても耐えられる構成はこれだけ。**
  * agent 側は `ca` / `client_cert` / `client_key` / `server_name` /
    `insecure_skip_verify`。`ca` は system root を置換せず追加する。
  * client 側の設定が 1 つでもあれば `host:port` は `https://` と解釈する。
  * TLS 材料が読めない場合、controller は **起動に失敗する**。
    平文で起動して暗号化されていると誤解されるのが最悪の失敗形。
  * 証明書の自動生成はしない。監視システムが trust anchor を発行すれば、
    誰も監査しない private CA が 1 つ増えるだけ。
  * agent の health endpoint は平文のまま。credential を運ばず、
    liveness 以外を明かさない。
* **診断結果の retraction 修正。** `diagnose_and_classify` が
  classification を追加しかしておらず、解決済みの障害の label が
  database に残り続けていた。古い label と生きた label は区別できないため、
  1 つでも古いものが混じれば board の label は全部意味を失う。
* **非標準ポートの設定**（新規機能）。
  `metadata.ports` は読まれていたが、どの provider も書いていなかったため、
  SSH が 22 以外のクラスタは設定不可能だった。
  * agent が `/etc/ssh/sshd_config` の `Port` / `ListenAddress host:port` を
    読んで自動検出し、controller へ報告する。
  * `[agent] ssh_port` で明示的に上書きできる。
  * agent がいない host は `[[entities]] ports = { ssh = 2222 }` で宣言する。
  * agent は自分の `listen` ポートも報告するため、
    変更しても peer が正しい場所を叩く。
* `docs/DEPLOYMENT.md` — 実クラスタ導入マニュアル。
* `docs/templates/` — コピーして使える設定テンプレート。
  テンプレートが `config check` を通ることをテストで強制する。

* v1 受け入れ手順の自動化（23 項目）。
  各段階で 1 つだけを壊し、それを指すことと **他を指さないこと** を検査する。
* `SENTINEL_AGENT_FAILURE` / `SSH_SERVICE_FAILURE` rule。
  いずれも「host が別経路で応答している」積極的証拠を要求する。
* CI 設定。fmt / clippy / test / release build は Docker 不要。
  疑似クラスタは別 job。
* `docs/VM_VALIDATION.md`。Docker で検証 **できない** 項目の一覧。

### M10 — Notification / Operations

* Notification は **変化があったときのみ** 送信する。
  継続中の incident は何度 polling しても送信しない。
  polling ごとの再通知は、監視を無視する習慣を作る。
* 重複排除は宛先ごと・trigger ごと。
  復旧通知が発生通知の重複として抑止されることはない。
  controller と fallback notifier が同じ incident を見ても通知は 1 回。
* 送信失敗は記録しないため、次回再試行される。
* Maintenance window は **通知のみ** を抑止する。
  observation・state・diagnosis は継続し、異常を healthy に書き換えない。
  影響 entity が **すべて** 対象の場合のみ抑止する。
* Webhook provider（ntfy / Gotify / Slack / Discord 等に POST 可能）。
  payload は自己記述的で、受信側が Sentinel の内部を知る必要がない。
* `sentinel install controller|agent` — hardening 済み systemd unit を生成。
  credential は unit に書かず、ファイルを参照する。
* `docs/OPERATIONS.md` を追加。段階的導入、診断結果の読み方、
  トラブルシューティングを記載。

### M9 — Diagnosis / Incident Correlation

* Incident engine。相関の原則は **「同じ症状」ではなく「同じ原因」**。
  fingerprint を suspected root entity から導出するため、
  1 つの fileserver に起因する複数の症状が 1 incident にまとまる。
  無関係な障害が同時刻に起きても統合されない。
* Dependency fan-out による severity 決定（`SPEC.md` §101）。
  3 台以上が依存する対象の障害は critical。
  低 confidence の推測は fan-out によらず critical にしない。
* Lifecycle: root が復旧しても依存先が未復旧なら `RECOVERING` に留まり、
  `RESOLVED` にしない（`IMPLEMENTATION.md` §75）。
* 再発は 30 分以内なら同一 incident を reopen し、
  flapping で毎回新規 alert を出さない。
* Incident・diagnosis・evidence・timeline の永続化。
  controller 再起動時は open な incident を再開し、再 alert しない。
* timeline は「変化」のみ記録する。polling ごとに 1 行増えると
  重要な出来事が埋もれるため。
* evidence は上限付き。発生時点のものを優先して保持する。
* `sentinel incident list` / `sentinel incident show` を追加。

### M8 — Peer Monitoring

* Peer assignment（`SPEC.md` §46-§49）。
  observer は「互いに似ていない」ことを優先して選ぶ:
  同一 dependency domain / 異なる domain / 依存を共有しない infrastructure。
  同じ storage の背後にいる observer 3 台は、視点 1 つを 3 回数えただけ。
* 決定的な割り当て。controller 再起動で全 observer が入れ替わり、
  debounce counter が一斉にリセットされることを防ぐ。
* `GET /v1/agents/{id}/assignments`。address は controller が inventory から解決し、
  agent が推測する余地を残さない。
* Agent は割り当てられた peer を観測し、observation に自分を署名する。
  controller 到達不能時も、既存の割り当てで観測を継続する。
* Reachability rule:
  * `HOST_UNREACHABLE` — 独立した observer が **全員** 失敗（2 台以上）。
  * `PATH_SPECIFIC_NETWORK_FAILURE` — 一部のみ失敗。
  * observer が 1 台だけなら **どちらも出さない**。
* `POWER_OFF` を主張しない。confidence は `High` 止まりで `Confirmed` にしない。
  電源断・NIC 故障・switch 障害はここからは同じに見える。
* `sentinel peers` を追加。observer が付いていない entity を明示する。

### M7 — GPU

* `gpu.nvidia` probe（`nvidia-smi` の CSV 出力を parse）。
  `[N/A]` / `[Not Supported]` を parse 失敗ではなく「値なし」として扱う。
* Agent は probe が実際に観測した GPU 数を registration へ反映する。
  capability（GPU を持ち得る）と観測結果（GPU が 2 枚ある）を混同しない。
* GPU 数の不一致は degraded として報告し、node を broken とは呼ばない。
  Slurm に GRES 行が無い場合は「不一致」ではなく「未記載」として扱う。
* GPU の状態を変更する `nvidia-smi` オプションを使わないことをテストで強制。
* 実 GPU の検証は level 4（VM / 実クラスタ）。container では mock のみ。

### M6 — Storage / NFS

* NFS probe を危険度で分割:
  * `nfs.client.mount` — `/proc/self/mounts` のみ。hang 中も安全。
  * `nfs.client.io` — 実 I/O。**mount ごとに同時 1 本**（`SPEC.md` §76）。
  * `nfs.server.port` / `nfs.server.exports` — 到達性と export 一覧。
* `NFS_OK` / `NFS_SLOW` / `NFS_TIMEOUT` / `NFS_STUCK` の区別。
* Storage 診断 rule:
  * `NFS_SERVICE_FAILURE` — host は稼働、export service のみ異常。
  * `SHARED_STORAGE_FAILURE` — 同一 storage の複数 client が同時異常。
    group は dependency graph から**導出**し、宣言しない。
  * `NFS_CLIENT_FAILURE` — 1 client のみ異常、同一 storage の peer は正常。
  最後の 2 つは相互排他であることをテストで強制。
* Agent は NFS mount ごとに probe を個別スケジュール（同時実行枠も個別）。
* Slurm capability 検出をより保守的に変更。
  `slurm.conf` が読めない host は Slurm role を主張しない。
  バイナリの存在だけでは fileserver が compute node を名乗ってしまう。
* `[controller] observe` を追加。controller 自身が remote probe を行うかの制御。

### M5 — Slurm Diagnosis

* Diagnosis engine: typed Rust rule、決定的評価、rule の panic 隔離。
  DSL も LLM も使わない（`SPEC.md` §94）。
* `DiagnosisContext`: rule が参照できるのは保存済みの
  inventory / state / observation のみ。
  外部通信もコマンド実行も行わないため、診断は後から再現・検証できる。
* Slurm rule:
  * `SLURM_ONLY_DEGRADATION` — host は健全、Slurm 上のみ DRAIN。
  * `SLURMD_SERVICE_FAILURE` — host は応答するが slurmd が登録しない。
  * `SLURM_CONTROL_PLANE_FAILURE` — control plane 自体の異常。
  * `RESOURCE_CONFIGURATION_MISMATCH` / `GPU_CONFIGURATION_MISMATCH`。
* すべての rule が「host が健全である積極的証拠」を要求する。
  証拠が無ければ診断を出さない。
  これが無いと、死んだ host が「死んだ daemon」として報告される。
* `sentinel diagnose` を追加。診断・根拠・read-only な調査コマンドを表示。
* 疑似クラスタで検証: drain / slurmd 停止 / host 停止 が
  それぞれ異なる結果（3 つ目は「診断を出さない」）になること。

### M4 — Basic Host Monitoring

* Probe runner: timeout、panic 隔離、target ごとの同時実行数制限。
  probe の panic が daemon を落とさないことをテストで検証。
* Probe 実装:
  * `host.metrics` — `/proc` から load / memory / pressure / uptime / boot ID。
    負荷は degraded であって failed ではない（計算 node は負荷が仕事）。
  * `network.tcp` — 到達性。**refusal は到達可能の証拠**（ADR 0004）。
  * `ssh.service` — banner 確認のみ。認証も remote 実行も行わない。
  * `systemd.unit` — `systemctl show` による read-only な状態取得。
  * `sentinel.agent` — peer から agent の health endpoint を確認。
* Agent の health endpoint（`/v1/agent/health`）。read-only、GET のみ。
  agent 本体をロックせずに応答するため、probe が遅くても peer から見える。
* Agent 側 local probe scheduler（probe ごとの interval と jitter）。
* Controller が observer として remote probe を実行。observation には observer を記録。
* State component（host / network / ssh / agent / service）への mapping。
  未 mapping の probe が生まれないことをテストで強制。
* 疑似クラスタで検証: agent 停止と sshd 停止が、
  それぞれ独立した component 異常として観測されること。

### M3 — Docker Compose 疑似クラスタ

* 6 container（controller 1 / compute 3 / fileserver 2）の疑似クラスタ。
  **実 munge / slurmctld / slurmd** が動作する。
* 障害注入シナリオ 11 種:
  `stop-agent` / `stop-slurmd` / `stop-ssh` / `drain-node` / `stop-fileserver` /
  `degrade-storage` / `pause-host` / `stop-host` / `stop-controller` /
  `isolate` / `recover-all`。すべて冪等で復旧可能。
* `wait-healthy` は container の起動だけでなく、controller API・Slurm の node 登録・
  storage service・agent 登録がすべて揃うまで待つ。
* Docker integration test 11 件（`SENTINEL_DOCKER_TESTS=1` で opt-in）。
  Docker 非対応環境でも unit / simulation テストは通る。
* Slurm role の判定を設定ベースへ変更（ADR 0003）。
  バイナリの存在では compute node が control plane を主張してしまう。
* capability の取り下げを provider 単位で反映（`reconcile_capabilities`）。
* `docs/SECURITY.md`、`dev/compose/README.md` を追加。

### M2 — Agent + Protocol

* Wire protocol v1（versioned JSON over HTTP）。binary version と protocol version を分離。
  未知 field を許容し、controller 更新が fleet 全体を落とさない設計。
* 認証: cluster-scoped bearer credential。未認証動作は提供しない。
  credential はファイルまたは環境変数から読み、バイナリへ埋め込まない。
  比較は constant-time、`Debug` 出力は常に redact。
* Controller HTTP API: `/v1/health`（無認証）、`/v1/agents/register`、
  `/v1/agents/heartbeat`、`/v1/observations/batch`。
  **コマンドを実行する route は存在しない**（テストで検査）。
* Agent session 管理: boot ID の変化のみを reboot と判定し、
  agent プロセスの再起動と区別する。
* Runtime discovery: `SystemInspector` 抽象により、
  実機に無い構成（GPU node、fileserver 等）に対してもテスト可能。
  capability の **不在** も明示的に記録し、role hint を上書きできるようにする。
* Local spool: WAL、age / rows / bytes による上限、順序付き replay、
  idempotent 再送、重要な行を優先保持。送信前に書く（ADR 0002）。
* Agent daemon: 登録・heartbeat・spool flush。controller 不在時も動作を継続。
* CLI: `sentinel controller`、`sentinel agent`、`sentinel doctor`。
* `docs/SECURITY.md` を追加。

### M1 — Passive Controller + Slurm

* 共通 external command runner: timeout、出力サイズ上限（truncate 事実の記録）、
  allowlist、構造化エラー。probe が個別に `Command::new()` を書くことを禁止。
* Slurm integration: `scontrol show nodes -o` / `show partitions -o` / `ping` の
  parser、hostlist 展開、node state（base + flag + suffix）の解釈。
  未知 field・未知 state に対して寛容。
* Inventory: 複数 provider からの merge、discovery source 保存、
  消失時の stale 化（削除しない）、endpoint 未発見の edge の遅延解決。
* Slurm inventory provider（Host / Service / Scheduler entity と依存 edge を生成）と
  static config provider。
* State engine: probe → component の宣言的 mapping、debounce、severity cap。
  再起動時は保存済み state から再開。
* 永続化: entity / capability / label / dependency / observation / state /
  transition の read-write。observation ingestion は idempotent。
* Passive controller: discovery cycle 1 回で inventory・observation・state を更新。
  provider が失敗しても他 provider の結果を消さない。
* CLI: `sentinel status`、`sentinel discover`、`sentinel entity list|show`、
  `sentinel dependency list`。全て `--json` 対応。
* `fixtures/slurm/` に実出力形式の fixture を追加し、parser テストの入力とする。

### M0 — Repository / Core Domain

* Core domain model を追加: `Environment` / `ManagedEntity` / `Capability` /
  `DependencyEdge` / `ProbeDefinition` / `Observation` / `EntityState` /
  `StateTransition` / `Diagnosis` / `Incident`。
* Entity identity を natural key からの UUIDv5 導出に決定（ADR 0001）。
* Capability 解決の優先順位を実装（force-disable > force-enable >
  runtime discovery > role hint）。
* Dependency を cycle 許容の汎用有向グラフとして実装。全 traversal に visited set。
* Debounce / hysteresis（既定: warning 2、critical 3、recovery 2）。
* 設定の読み込み・優先順位追跡・検証。未知の `config_version` は拒否。
* SQLite core schema の初期 migration。controller 複数台を禁止しない設計。
* CLI skeleton: `sentinel version`、`sentinel config check`、`sentinel config show`。
* `src/` へ deployment 固有名が混入していないことを検査するテスト。
