# Ansible による展開

計算ノードが 10 台を超えると、手作業での配布は現実的ではありません。
このロールは [DEPLOYMENT.md](../../docs/DEPLOYMENT.md) の §3〜§6 を自動化します。

**Sentinel 側に Ansible 固有のものは一切ありません。** ここにあるのは
「バイナリを置き、`sentinel install` を実行し、設定を配る」だけです。
別の構成管理ツールを使っているなら、同じ手順を移植してください。

## 前提

* controller に Ansible がインストールされている
* controller から各ノードへ SSH でログインでき、`sudo` が使える
* controller 側で `sentinel install controller` が済んでおり、
  `/etc/sentinel/token` が存在する

## パスワードが必要な場合

SSH にも `sudo` にもパスワードが要る、という環境は珍しくありません。両方扱えます。

```bash
ansible-playbook -i inventory.ini site.yml --ask-pass --ask-become-pass
```

| オプション | 何のパスワードか | 備考 |
| --- | --- | --- |
| `--ask-pass` | SSH ログイン | `sshpass` が必要（`apt install sshpass`） |
| `--ask-become-pass`（`-K`） | `sudo` | |

実行開始時に 1 回ずつ聞かれ、以降は全ノードで使い回されます。
**全ノードで同じパスワードであることが前提**です。

### SSH は鍵にすることを強く推奨します

パスワード認証は毎タスクで使われるうえ、`sshpass` は
Ansible 公式が非推奨としており、`PasswordAuthentication no` の環境では
そもそも使えません。**鍵を配るのは 1 回で済みます。**

```bash
ssh-keygen -t ed25519 -C "ansible@controller"    # まだ無ければ
for n in node01 node02 node03; do ssh-copy-id "$n"; done
```

これで `--ask-pass` が不要になり、`sudo` のパスワードだけになります。

```bash
ansible-playbook -i inventory.ini site.yml -K
```

### sudo もパスワード無しにする場合

これは各サイトのセキュリティ方針次第です。行うなら、
**このロールが使うコマンドだけに限定**してください。

```
# /etc/sudoers.d/ansible-sentinel
%wheel ALL=(ALL) NOPASSWD: /usr/local/bin/sentinel
```

全 `sudo` を NOPASSWD にする必要はありません。

### ノードごとにパスワードが違う場合

`--ask-pass` は 1 つしか受け付けません。`ansible-vault` で
ホスト変数として持たせてください。

```bash
ansible-vault create host_vars/node01.yml
```

```yaml
ansible_password: "..."
ansible_become_password: "..."
```

```bash
ansible-playbook -i inventory.ini site.yml --ask-vault-pass
```

**平文の `inventory.ini` にパスワードを書かないでください。**
このリポジトリに commit されます。

### credential の読み取りについて

`/etc/sentinel/token` は mode 0400、`sentinel` ユーザー所有です。
ロールは **controller 上で root として読み取り**、各ノードへ配ります
（`slurp` + `become: true` + `delegate_to: localhost`）。
`-K` を渡していれば、この読み取りにもそのパスワードが使われます。

## 使い方

```bash
cd deploy/ansible
cp inventory.example.ini inventory.ini
$EDITOR inventory.ini          # ノード名と変数を書く
ansible-playbook -i inventory.ini site.yml
```

**まず 1 台で試してください。**

```bash
ansible-playbook -i inventory.ini site.yml --limit node02
```

`--check` を付けると、何も変更せずに差分だけ確認できます。

## このロールがすること

| 手順 | 対応する節 |
| --- | --- |
| release からバイナリを取得（アーキテクチャ別） | §3.1 |
| `/usr/local/bin/sentinel` に配置 | §3.2 |
| `sentinel` サービスユーザーを作成 | §6.1 |
| `sentinel install agent` を実行 | §6.1 |
| cluster credential を配置（0400） | §6.2 |
| 設定ファイルを配置 | §6.3 |
| `sentinel config check` で検証 | §6.4 |
| サービスを起動 | §6.4 |

**しないこと:**

* controller の構築（1 台なので手で行ってください）
* `[[entities]]` や依存関係の宣言（controller 側の設定）
* NIC の自動選択（`sentinel_interface` で指定してください）

## 変数

| 変数 | 既定 | 意味 |
| --- | --- | --- |
| `sentinel_version` | `v0.3.0` | 取得する release |
| `sentinel_environment` | *(必須)* | controller と一致させる |
| `sentinel_controller_address` | *(必須)* | `host:port` |
| `sentinel_interface` | *(未設定)* | クラスタ内通信の NIC 名 |
| `sentinel_roles` | `[]` | UI 上のグループ分け |
| `sentinel_observer` | `false` | この host を peer observer にするか |
| `sentinel_token_source` | `/etc/sentinel/token` | controller 上の credential |
| `sentinel_download_dir` | `/tmp` | 一時ファイルの置き場 |

## 検証

```bash
ansible -i inventory.ini agents -b -a "sentinel doctor"
sudo -u sentinel sentinel status
sudo -u sentinel sentinel peers
```

`sentinel doctor` の報告アドレスに `!` の警告が出ていないか確認してください。
出ていれば `sentinel_interface` を設定して再実行します。

## アップグレード

`sentinel_version` を変えて再実行します。設定ファイルと credential は
`sentinel install` が上書きしないため、そのまま残ります。

```bash
ansible-playbook -i inventory.ini site.yml -e sentinel_version=v0.3.1
```

**controller を先に更新してください。** protocol version が同じであれば
混在状態でも動作します。
