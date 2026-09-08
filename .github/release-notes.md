分散クラスタの監視・障害検知・原因診断システムです。
成果物は **単一バイナリ 1 つ**で、controller・agent・CLI をすべて兼ねます。

## ダウンロード

| ファイル | 対象 |
| --- | --- |
| `sentinel-x86_64-unknown-linux-musl` | x86_64 |
| `sentinel-aarch64-unknown-linux-musl` | ARM64 |

**静的リンク**なので、glibc のバージョンに関係なくどのディストリビューションでも動きます。
同一アーキテクチャなら全 host に同じファイルを配れます。

```bash
sha256sum -c sentinel-x86_64-unknown-linux-musl.sha256
sudo install -m 0755 sentinel-x86_64-unknown-linux-musl /usr/local/bin/sentinel
sentinel version
```

## v0.3.2 での修正

* **`sentinel install --force` が cluster credential を上書きしなくなりました。**
  `--force` を実行する理由は、多くの場合 systemd unit の更新を取り込むことです。
  それが credential を再生成していたため、**アップグレードのつもりで実行すると
  全 agent が一斉に締め出されていました。** 失敗は後から各ノードの認証エラーとして
  現れるため、原因に辿り着きにくい形でした。
  意図的な更新は、ファイルを削除して `install` を実行し、全 host に配り直します。
* アップグレード手順を [OPERATIONS.md](docs/OPERATIONS.md) に具体化しました
  （置き換え前の `config check`、controller → agent の順序、
  unit が変わった場合、切り戻し）。
* Ansible ロールが SSH / `sudo` のパスワード認証環境で動くようになりました。

## v0.3.1 での修正

**v0.3.0 には、実機で問題になる不具合が含まれています。更新を推奨します。**

* **到達性 probe が 1 つも動いていませんでした。** Slurm discovery で見つかった
  host は `slurm.compute` capability しか持たないため、`network.tcp` を
  要求していた reachability probe が全て skip されていました。
  結果、**誰も接触していない host が HEALTHY と表示されます**。
* **報告アドレスの選択。** loopback インターフェース上のアドレスを除外し、
  物理 NIC を仮想 NIC より優先。NIC が複数ある場合は「曖昧である」と報告します
  （`[agent] interface` で指定してください）。
* systemd unit が `StateDirectory=` を持つため、`/var/lib/sentinel` の
  手動作成が不要になりました。
* ダウンロードしたディレクトリのまま `install` すると、unit が
  消えるパスを指してしまう問題を警告するようになりました。
* 設定ファイルの `Permission denied` が、正しい実行方法を案内します。
* `install` が `scontrol` の有無を見て Slurm discovery を設定します。

ノードが多い場合は [Ansible ロール](deploy/ansible/) を使ってください。

## 導入

```bash
sudo sentinel install controller   # または agent
```

設定ファイル・systemd unit・cluster credential が生成されます。
書き換えが必要なのは `CHANGE-ME` を含む行だけです
（controller は 1 行、agent は 2 行）。

**NIC が複数ある環境では、各 host で `sentinel doctor` を 1 回確認してください。**
どの NIC でクラスタ内通信をしているかは自動判別できないため、
候補が複数ある場合はその旨が表示されます。`[agent] interface` で指定します。

手順の全体は [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) にあります。

## 何ができるか

単一の観測点からは区別できない障害を、複数の観測者の合意と依存グラフから切り分けます。

* host の死 / 経路だけの障害 / SSH だけの障害 / agent だけの障害 / slurmd だけの障害
* Slurm の DRAIN・DOWN と、host 自体の障害
* Slurm control plane の障害
* NFS server の障害 / client 側だけの障害 / 共有ストレージ起因の多ノード障害
* GPU のリソース設定不一致
* kernel event（OOM・I/O error・hung task・NVMe timeout・GPU Xid など）の継続収集

## 設計上の約束

* **単一の観測者の失敗から host の死を結論しません。** 独立した 2 つ以上の合意が必要です
* **BMC/IPMI の証拠なしに電源断とは言いません**
* **自動復旧を一切行いません。** reboot も restart も `scontrol update` もしません
* **リモート実行の口がありません。** SSH はバナーを読むだけで、鍵もパスワードも持ちません
* **診断に LLM を使いません**
* core に hostname・IP・Slurm パーティション・NFS 構成をハードコードしていません

## この版で検証していないもの

Docker 疑似クラスタで実 Slurm を動かした受け入れ 23 項目は通っていますが、
以下は**実機での検証が必要**です（[docs/VM_VALIDATION.md](docs/VM_VALIDATION.md)）。

* reboot をまたぐ boot ID の変化
* NFS hard mount 時の kernel D-state
* 実 GPU
* `journal.events` probe（container に systemd が無いため疑似クラスタでは動きません）
* TLS の実運用（protocol 部分は検証済み）

controller の HA、リモート読み取り API、per-node credential は範囲外です。
