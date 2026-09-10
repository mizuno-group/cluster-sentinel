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

## v0.3.23

`sentinel audit` を実クラスタ（16 host）で初めて走らせた結果の 3 件。
2 件は audit 自身の問題、1 件は audit が正しく見つけた本物の穴。

* **`probe_last_seen` が 6.9 秒かかっていた**（性能バグ）。
  `status` は毎回このクエリを走らせるため、
  **いちばんよく使うコマンドに 7 秒を足していた。**
  `(target, probe)` でグループ化しているのに、既存の index は
  `(target, finished_at)` と `(probe, finished_at)` で、
  **どちらも使えず 2 週間分の観測を全走査していた。**
  * グループ化に一致する index を追加。
  * JOIN を `IN` に変更（JOIN だと SQLite が GROUP BY 用の
    一時 B-tree を作るが、`IN` なら index を歩くだけで済む）。
  * 120 万件で実測: **3.06s → 0.64s（index）→ 0.14s（+ IN）。**

* **`systemd.unit` を全 host に対して「沈黙している」と報告していた**
  （audit の誤検知）。この probe は host も targeting に含むが、
  agent は **service entity ごとに**スケジュールし、
  観測は service に紐づく。host に聞けば当然「一度も無い」になる。
  **16 件中 16 件が誤検知の報告は、報告として死んでいる。**
  * catalog に「何を測れるか」と「何を測るよう配線されているか」を
    分けるフィールドを追加し、audit は後者を見る。
    この 2 つの距離こそ `nfs.server.exports` を隠していたもの。

* **controller 自身のホストの NFS ポートを誰も見ていなかった**
  （audit が見つけた本物の穴）。
  `nfs.server.port` は controller しか実行せず、controller は
  **自分自身のホストには一切 probe を打たない**
  （到達性については正当な除外だが、それが全 probe に及んでいた）。
  * peer agent も `nfs.server.port` を実行するようにした。
    対象の capability で gate されるので、実際に export している
    host にしか飛ばない。
  * これは穴を塞ぐだけでなく**証拠としても強い** — peer が互いの 2049 を
    見るのは、peer monitoring 全体が依拠している
    「独立した視点」の議論そのもの。

## v0.3.22

* **`sentinel audit`（新規）— 動いているはずの検査が動いているか。**

  `explain probes` は「何が動く**はず**か」、`entity observations` は
  「何が動い**た**か」を言うが、**この 2 つを突き合わせるものが無かった。**
  そしてその隙間は構造的に見えない: probe が走らなければ観測が生まれず、
  観測が無ければ失敗もせず、失敗しなければ診断もされない。
  **何も言われないので、すべて健全に見える。**
  `nfs.server.exports` が数か月気づかれなかったのはこれが理由。

  * 各 entity について、有効で・適用され・実行する主体があるのに
    自分の間隔どおりに報告していない probe を挙げる。
  * **probe ごとにまとめて表示する。** 1 台だけ静かなのはそのホストの
    問題で `status` が既に言うが、**全ホストで静かなのは配線されていない
    probe** であり、他のどこにも現れない。
  * 「一度も観測が無い」と「以前は動いていたが止まった」を区別する。
  * **静かでなければ使われない**ので、次は報告しない:
    意図的に無効化された probe、agent が居ないホストのローカル probe、
    observer が付いていないホストのリモート probe（それは
    `explain paths` の領分で、ここで挙げると直すべき場所を取り違える）、
    1 回取りこぼしただけのもの（probe 間隔の 10 倍、最低 5 分待つ）。
  * 沈黙があれば **exit code 2**。cron や CI に置ける。
  * **`status` の末尾にも 1 行出る。** この検査自体を実行し忘れれば
    同じことなので、聞かれなくても言う。

  検証: agent の schedule から export probe を再び外して当時の状況を
  再現したところ、`status` が自発的に警告し、`audit` が
  `nfs.server.exports` を両 fileserver で名指しした。戻すと沈黙する。

## v0.3.21

* **v0.3.20 が誤検知を出した**（バグ修正）。
  実機の head node に
  `CRITICAL parent is up but the port answers but nothing is exported`
  が出た。parent は NFS を **export していない**（5 台からマウントする側）。

  `storage.nfs.server` capability は
  「`/etc/exports` が存在する、または `exportfs` が入っている」で判定される。
  NFS クライアントとしてパッケージが入っていれば真になる。
  そこへ v0.3.20 で export probe が走るようになり、
  probe が「export ゼロ」を failure として報告したため、
  **「NFS を提供できる」ホストが「NFS が壊れている」ホストとして報告された。**

  * **probe は事実を述べ、ルールが判断する**という原則に戻した
    （マウントの `ro` 判定で一度通した整理と同じ）。
    export ゼロは `export_count: 0` という事実であって failure ではない。
    ホストの storage 状態も汚さない。
  * **export ゼロが障害になるのは 2049 で何かが listen している場合だけ。**
    そのときクライアントは接続できて拒否される — このルールが
    名指ししたい、あの分かりにくい障害。誰も listen していなければ、
    そのホストは単に NFS サーバではない。
  * export が消えた本物の fileserver（nfsd は生きている）は
    従来どおり検知される。
  * メッセージが `... is up but the port answers but nothing is exported` と
    but が 2 回出て壊れていた。しかも「ポートが応答している」を
    確認せずに主張していた。1 文として読めるように直し、
    主張は実際の観測に合わせた。

## v0.3.20

`nfs.server.exports` の観測が 1 件も無い、という調査から 3 件。
いずれも実機（ZFS の `sharenfs` で export している fileserver）で発覚。

* **`nfs.server.exports` は、誰の環境でも一度も走っていなかった。**
  probe は定義され、capability で gate され、catalog に載り、
  診断ルールからも参照されていた。**実行経路だけが無かった。**
  `ExecutionMode::Local` なので走れるのは agent 上だけだが、
  agent の `schedule_storage_probes` は client 側の 2 本しか登録しておらず、
  **走る場所が存在しなかった。**
  * その結果、`NFS_SERVICE_FAILURE` の
    「ポートは応答するが何も export されていない」という分岐は
    **本番で到達不能**だった。storage の健全性も
    「2049 に何か応答する」だけで決まっていた。
  * client 側の early return（マウントが無ければ return）より**前**に登録する。
    マウントを 1 つも持たない純粋な fileserver は、
    まさにそこで弾かれるホストなので。

* **`/etc/exports` しか読んでいなかった。**
  ZFS の `sharenfs` は `/etc/exports.d/zfs.exports` に書き、
  隣の `/etc/exports` はパッケージ同梱のコメントだけのファイルになる。
  **プール全体を export している健全な fileserver が
  「何も export していない」と報告される。** 上の修正だけを入れていたら、
  全 fileserver に critical が出ていた。
  * `exportfs` が読むもの（exports(5)）と同じく、
    `/etc/exports` と `/etc/exports.d/*.exports` の両方を読む。
    ZFS 固有の対処ではない。
  * **読めないソースは「export が無い」ではない。**
    権限で読めないファイルがあるときは結論を出さない（Unsupported）。
    実機の `/etc/exports.d/` には root のみ読み取り可の
    `zfs.exports.lock` が同居していた。
    拡張子 `.exports` のみを読むので実害は無かったが、判定は明示した。
  * catalog の記述も訂正。この probe は `exportfs -v` を実行しない。
    ファイルを読むだけ。

* **entity の参照が曖昧だった。**
  導出された storage domain は提供元ホストの名前をそのまま名乗るため、
  `david02` が 2 つの entity を指すようになっていた。
  各コマンドが自分の並び順で先頭を黙って選んでおり、
  **`entity show david02` は storage を、
  `entity observations david02` は host を返していた。**
  * `host/david02` のような `type/name` を受け付ける
    （依存関係や設定ファイルと同じ書式）。
  * 曖昧なときは黙って選ばず、候補を挙げて拒否する。

## v0.3.19

storage の健全性が UNKNOWN のままだという報告を追った結果、3 件。

* **agent の報告した観測が storage 導出を経由していなかった**（バグ修正）。
  観測が controller に届く経路は 3 つあり、そのうち
  **agent のバッチだけが state engine を直接叩いていた。**
  `nfs.server.exports` は local 専用の probe なので、
  **agent のバッチが唯一の経路**であり、
  最も権威のある証拠が丸ごと storage entity に届いていなかった。
  * 3 経路を 1 箇所に集約した。

* **`entity observations --probe X` が「直近 N 件」の中だけを探していた。**
  probe の絞り込みが取得の**後**に行われていたため、
  上限 40 件は全 probe に対して効く。
  observer が 3 台付いた host では 40 件は 1 分未満で、
  **60 秒間隔の probe はまず見えない。**
  そして出力は「観測が記録されていません」と言う。
  これは事実と違うだけでなく、はるかに深刻に聞こえる。
  * クエリ側で絞り込むようにした。
  * 空のときのメッセージを 2 つに分けた。
    「この probe は一度も走っていない」は capability を見に行く話で、
    「この entity は一切監視されていない」は全く別の話なので。

* **テストが約 4 回に 1 回落ちていた**（flaky test の修正）。
  同一マイクロ秒に作られた 2 つの観測は `finished_at` が同値になり、
  クエリはランダムな UUID で順序を決める。
  コイン投げで合否が決まっていた。明示的な時刻を与えて固定。

## v0.3.18

* **NFS 導出が「直近 N 件の観測」を読んでいた**（バグ修正）。
  v0.3.17 導入直後の実機で発覚。`status` の Storage 5 件のうち
  **4 件が UNKNOWN (stale)** になった。

  stale になった 4 件は、いずれも**マウントしているのが 1 台だけ**の
  storage domain だった。その 1 台は agent を入れたばかりの controller
  host で、peer observer が 3 台付いたところだった。

  reachability は observer ごとに 5 秒間隔、マウント表の報告は 30 秒間隔。
  「このホストの直近 N 件」という窓は速い probe で 1 分足らずで埋まり、
  マウント表を追い出す。**マウント表に言及しない snapshot は
  「その storage domain は無くなった」と言う snapshot** なので、
  グラフがマウントの変化ではなく窓からの追い出され方に同期して明滅していた。

  * probe ごとの最新 1 件を読むようにした
    （v0.3.15 で診断側に入れたのと同じ修正が、導出側に残っていた）。
  * **こちらには鮮度の上限を付けない。** 「このホストが何を使っているか」
    への最良の答えは最後に判明したマウント表であり、
    報告が止まった agent は「そのホストの健全性」の問題として
    別に扱われる。忘れることで二重に、しかも悪く答える必要はない。

## v0.3.17

実機導入で見つかった 3 件。いずれも「間違ったことを言う」ではなく
「必要なことを言わない」型の問題。

* **storage entity の健全性が永久に UNKNOWN だった。**
  storage は fileserver そのものとは別概念として意図的に分けてある
  （「マシンは生きているが export だけ落ちた」を表現するため）ので、
  直接 probe されるものが無い。実クラスタでは
  `status` に何も言わない行が 5 つ並ぶ状態になっていた。
  * 必要な証拠は提供元ホストの export probe として既に存在するので、
    その観測を storage entity にも向けるようにした。
  * **server 側の観測だけを使う。** ホストは fileserver でありながら
    NFS クライアントでもありうる（scratch を export しつつ他所の home を
    マウントする計算ノード）。両方が同じ `storage` component に入るため、
    component をそのまま写すと**クライアント側のマウント詰まりが
    「このホストの export が壊れた」として報告される。**
    それは人を間違ったマシンに送ることになる。
  * state engine を通すので debounce は他と同じ。
    元の観測 ID を引き継ぐため、判定から実在する観測まで辿れる。
    DB に行は増えない。
  * probe されていない提供元の storage は UNKNOWN のまま。
    推測より正直な答えなので。

* **`install` が書いたファイルをサービスユーザーが読めなかった。**
  設定は root 所有・0640 で書かれるが unit は `User=sentinel` で動く。
  controller が動いている host に agent を追加すると、
  **systemd からは再起動ループとしか見えない**状態になる。
  権限の話はどこにも出ない。
  * サービスユーザーが既にあれば、書いたファイルの所有者を設定する。
  * **既にあるものを「作れ」と言わない。** 従来は手順 1 が
    「サービスユーザーを作る」で、既にある host ではその手順ごと
    飛ばされる。そこに新しいファイルに必要な chown が埋まっていた。
    手順 2 の「credential を配置する」も同様で、そのまま従えば
    **動いている credential を上書きして全 agent を締め出す。**
  * まっさらな host では従来どおり全手順を出す。

* **`entity show` が、host の申告したハードウェアを表示していなかった。**
  scheduler の設定と突き合わせる際の片側そのものなので、
  「Slurm は 1 GPU を期待、host は 0 と報告」を
  host の実際の申告と照合する手段が無かった。
  * 数えていない場合は `(not stated)` と表示する。
    0 とは別物として扱う。

## v0.3.16

* **v0.3.15 の観測ウィンドウ変更に伴う regression の修正。**
  pseudo-cluster の acceptance が捕まえた
  （`HOST_UNREACHABLE was not detected (saw: PATH_SPECIFIC_NETWORK_FAILURE)`）。

  「直近 32 件」というウィンドウは、**読み込む量**と
  **どれだけ古い観測まで採用するか**という別々の 2 つを
  たまたま同時に縛っていた。v0.3.15 は前者を
  「probe ごと・observer ごとの最新 1 件」に直したが、
  後者を一緒に外してしまっていた。

  結果、**host を完全に停止したのに「経路障害」と報告された。**
  その host の観測をやめた observer の「到達できた」という
  最後の回答が、永久に最新のまま残るため。

  * 観測の鮮度を、**その probe 自身の間隔**を基準に判定するようにした。
    古さは相対的で、1 分前の reachability の回答には価値がないが、
    1 分前の Slurm node view は現在の値。件数によるウィンドウでは
    これを表現できない。
  * 既定では probe 間隔の 4 倍を過ぎた観測は証拠として採用しない
    （reachability なら 20 秒、Slurm node view なら 20 分）。
    1 回落とすのは揺らぎ、4 回落とすのは probe が止まっている。

## v0.3.15

実クラスタで「GPU が 1 のはずが 0」という通知が頻発し、
すぐ RESOLVED になる、という報告からの修正。誤検知で、原因は 3 つ重なっていた。

* **agent が「GPU はあります、0 台です」と登録していた。**
  registration が GPU capability の有無だけを見て、
  台数には常に `0` を入れて送っていた（数えていなかった）。
  scheduler との突き合わせルールはそれを信じるので、
  **agent が入っている GPU node は自分の Gres 行と恒久的に食い違う。**
  * probe が実際に数えた台数を報告するようにした。
    まだ一度も数えていなければ「言わない」（`None`）。
    「未申告」は比較対象なしとして扱われるため、誤検知にならない。

* **`nvidia-smi` の実行に失敗したとき、probe が `gpu_count: 0` と報告していた。**
  数えられなかったことと 0 台だったことは違う。
  driver が一瞬取り込み中だっただけで
  「カードが消えて戻ってきた」ように見えていた。
  * 失敗時は台数を書かない。証拠が無いことは、無いことの証拠ではない。

* **診断が「直近 32 件の観測」を見ていた（flapping の正体）。**
  reachability は observer ごとに 5 秒間隔、Slurm の node view は 5 分間隔。
  固定長の窓は速い probe で埋まり、遅い probe を追い出す。
  そのため scheduler 側の値は「届いた直後の数十秒」しか見えず、
  診断が cluster の状態ではなく**窓からの追い出され方に同期して**
  現れたり消えたりする。これが「通知が来てすぐ RESOLVED」の正体。
  * probe ごと・observer ごとの最新 1 件を読むようにした。
    観測の頻度が違っても取りこぼさない。
  * GPU に限らず、**Slurm 由来のすべての診断**が影響を受けていた。

## v0.3.14

* **NFS の依存関係を、agent が報告するマウント表から自動導出するようにした。**
  実クラスタで、11 node × 5 fileserver の構成を表現するのに
  `[[dependencies]]` を 24 個手書きする必要があった。
  それは cluster が既に知っている事実の 2 つ目の写しであり、
  **写しがずれても何も言わない。**
  診断が「fileserver 1 台を名指しする incident 1 件」から
  「client ごとに 1 件」に静かに劣化し、それが分かるのは
  それを必要とした障害の最中になる。

  * `nfs.client.mount` の観測から、fileserver の host entity、
    storage entity、`provides`、`uses_storage` をすべて導出する。
    マウント構成が変わっても設定ファイルを触る必要がない。
  * **address から entity を作ることはしない。** マウントが
    `10.0.0.4:/data` と書かれていて、その address を持つ host を
    知らない場合は、entity を捏造せず「解決できなかった」と報告する。
    address は identity ではない（ADR 0001）。
    `sentinel discover` がどの IP をどの node が使っているかを示す。
  * 名前で書かれていれば host を作る。`filesrv01:/data` は
    「filesrv01 という機械が存在して storage を提供している」という証拠で、
    それはまさに設定ファイルに書かせていた内容そのもの。
  * **導出された edge は retract もされる。** node が別の fileserver に
    移れば古い edge は消える。増える一方のグラフは、
    もはやマウントしていない fileserver の障害に投票し続ける。
  * 手書きの宣言は併存する（`[discovery.nfs] enabled = false` で完全停止）。

* **refusal しか無い状況を「経路障害」と診断していた**（バグ修正）。
  実クラスタで誤検知。SSH が 22 以外に移されていたため probe は
  誰も listen していないポートを叩いていた。2 台の observer は
  カーネルが RST を返して `refused`（= 到達、RST を返すのは生きている証拠）、
  1 台は firewall が DROP して timeout。
  **閉じたポートに対する RST と DROP の差**、つまり firewall の設定差だけで
  head node への経路障害が CRITICAL で報告され続けていた。
  * `PATH_SPECIFIC_NETWORK_FAILURE` は、**少なくとも 1 台の observer が
    実際に接続を完了している**ことを条件にした。
    誰も接続できていないなら、そこに「壊れた経路」は無い。

* **`sentinel status` が incident を一切表示していなかった**。
  2 台の host 間の経路障害はどの entity にも属さないため、
  全 host が HEALTHY、集計行も「31 healthy」と表示される一方で、
  `sentinel incident list` には open な CRITICAL がある、という状態になる。
  `status` だけを見ている人には知る術がなかった。
  * open な incident を entity 一覧の前に表示する。
  * open な incident があれば exit code も非ゼロになる。

* **`ok / refused` という観測表示が誤解を招いていた**。
  `refused (so the host answered)` と、何を証明したのかを書くようにした。

* **一度も通知されなかった incident が、永久に通知されないままになる**（バグ修正）。
  実クラスタで発見。`sentinel incident list` に open な CRITICAL が出ているのに、
  webhook には何も届いていない状態が続いていた。

  incident が announce される機会は「それを open した 15 秒のパス」**1 回きり**だった。
  そこを逃すと二度と来ない:
  * まだ webhook を設定していなかった
  * webhook が 500 を返した（コード上は「次のパスで再送する」と書かれていたが、
    次のパスにその incident はもう乗っていなかった）
  * そのパスの直後に controller が再起動した

  再起動が効くのは、controller が起動時に open な incident を engine に seed するため。
  これ自体は正しい（再起動のたびに対応中の障害を再通知しては困る）が、
  **「もう伝えた」と「まだ一度も伝えられていない」を区別する情報がどこにも無かった。**
  外から見ればどちらも同じ沈黙で、正しいのは片方だけ。

  * 通知の配信記録を永続化するようにした。`notifications` テーブルは
    schema には最初からあったが、**一度も書かれていなかった。**
  * controller 起動時に配信記録から deduplicator を復元する。
  * 通知の候補を「このパスで変化したもの」から
    「まだ伝えていないもの」に変えた。open な incident は毎パス候補に上がり、
    実際に送るかどうかは deduplicator が判断する。
  * open の通知は incident ごとに 1 回だけ（lease で期限切れしない）。
    incident が resolve した時点で記録を消すため、
    同じ障害が後日再発したときはきちんと鳴る。
  * 配信失敗が本当に次のパスで再送されるようになった。
    コメントが主張していたことに実装が追いついた。

* **`min_interval` と `format` が生成される config に出ていなかった**。
  v0.3.12 で追加した設定が `sentinel install` / `sentinel config init` の
  出力にも `docs/templates/controller.toml` にも書かれておらず、
  **新規に導入した人はその存在を知る手段がなかった**。
  webhook への送信間隔を制限したいという状況は、たいてい
  制限が要ると気づいた後ではなく先に来るので、既定値が見えている必要がある。
  * 通知セクションを手書きの固定文字列から、
    `Config::default()` から描画する形に変更。以後は既定値と一緒に動く。
  * コメントを外した通知ブロックが実際に parse され、
    書かれている値がコンパイル時の既定値と一致することをテストで固定した。
  * 併せて宛先ごとの `format`（`"generic"` / `"slack"`）も記載。

## v0.3.13

* **`sentinel explain`**（新規）。この仕組みを**作っていない人**が読むためのもの。
  `status` は「何を結論したか」を言うが、「それがどうやって分かるのか」は
  どこにも出ていなかった。**根拠を確かめられない監視は、
  信じるしかない監視であり、誰にも訂正できない。**
  * `explain capabilities` — 各 capability の意味と、
    **host に対して何を検査して判定しているか**（ファイルの有無、
    `PATH` 上のプログラム、設定ファイルの記述）、
    そしてどの probe を有効にするか。
  * `explain probes` — 各 probe が**実際に実行するコマンド**または syscall、
    必要な capability、どこが実行するか、間隔と timeout。
    **cadence は設定を反映した値**で、停止中なら `[DISABLED]` と出る。
  * `explain paths` — 監視経路。host ごとに、到達アドレス、
    自分の agent が実行する probe、他所から実行される probe、観測者の一覧。
  * `Either` の probe を「自分自身から」に混ぜない。
    それは他所から実行されるものであり、agent が自分に応答を訊いても
    「はい」以外を返しようがない。
* `entity show` と `doctor` から `explain` への導線を追加。

## v0.3.12

* **`[notification] min_interval`**（新規、既定 `1s`）。
  同一宛先への送信間隔の下限。**間引くのではなく間隔を空ける。**
  1 つの障害が依存先を巻き込むと 1 回の診断で複数の通知が発生し、
  webhook は共有された rate-limited な資源
  （Slack は概ね毎秒 1 通で、超えると 429）。
  落とすと、落ちたのが肝心の 1 通かもしれない。

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
