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

## どのユーザーで SSH するか

**ユーザー名は秘密情報ではないので、`ansible-vault` は要りません。**
決まる順序は次のとおりです。

| 優先 | 指定方法 |
| --- | --- |
| 1 | inventory の `ansible_user = li` |
| 2 | `ansible-playbook -u li` |
| 3 | `ansible.cfg` の `remote_user` |
| 4 | **`~/.ssh/config` の `User`** |
| 5 | 実行している人のローカルユーザー名 |

Ansible は既定で `ssh` コマンドを使うため、**`~/.ssh/config` をそのまま尊重します。**
普段 `ssh node01` で入れているなら、Ansible も同じ設定で入ります。
inventory に何も書かなくて済むので、これが一番きれいです。

```
# ~/.ssh/config
Host node* filesrv*
    User li
    Port 22
```

疎通確認:

```bash
ansible -i inventory.ini agents -m ping -K
```

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

### ansible-vault が必要なのはどこか

| 状況 | 必要なもの |
| --- | --- |
| SSH 鍵、`sudo` パスワード共通 | `-K` だけ |
| SSH もパスワード、両方共通 | `--ask-pass -K`（+ `sshpass`） |
| **ノードごとに `sudo` パスワードが違う** | **ansible-vault**（下記。1 ファイルで済みます） |
| ユーザー名がノードごとに違う | `~/.ssh/config` か `ansible_user`（vault 不要） |

クラスタは通常、全ノードで同じアカウント・同じパスワードなので、
**`--ask-pass -K` で足ります。** vault が要るのはパスワードが分かれている場合だけです。

### ノードごとに sudo パスワードが違う場合

`--ask-become-pass` は 1 つしか受け付けないので、ここが vault の出番です。
**ただしノード 1 台につき 1 ファイル作る必要はありません。**
暗号化ファイル 1 つに全ノード分を辞書で持たせます。

`group_vars/agents/vars.yml`（平文、commit してよい）:

```yaml
ansible_become_password: "{{ vault_become_passwords[inventory_hostname] }}"
```

`group_vars/agents/vault.yml`（暗号化）:

```yaml
vault_become_passwords:
  node01: "..."
  node02: "..."
  fileserver01: "..."
```

作り方:

```bash
cp group_vars/agents/vault.yml.example group_vars/agents/vault.yml
$EDITOR group_vars/agents/vault.yml          # 実際のパスワードを書く
ansible-vault encrypt group_vars/agents/vault.yml
```

以降の編集は `ansible-vault edit group_vars/agents/vault.yml` で行います
（自動で復号し、保存時に再暗号化します）。

実行:

```bash
ansible-playbook -i inventory.ini site.yml --ask-vault-pass -K
```

**2 ファイルに分けるのは慣習です。** 暗号化ファイルは `git diff` でも
`grep` でも中身が見えないので、「どの変数がどこから来るのか」を
平文側に残しておくと後から読めます。

#### `-K` も併せて必要です

credential を読むタスクは **controller 上で root として実行**します
（`delegate_to: localhost` + `become: true`）。
localhost は inventory に居ないので `vault_become_passwords` が効かず、
ここだけ `--ask-become-pass` の値が使われます。

`ansible_become_password` 変数が設定されているホストではそちらが優先されるため、
**`-K` で入力した値は controller 用、vault の値は各ノード用**、と自然に分かれます。

controller の sudo パスワードを入力したくない場合は、
credential の複製を自分で読める場所に置き、そちらを指してください。

```bash
sudo cp /etc/sentinel/token ~/sentinel-token
sudo chown "$USER" ~/sentinel-token && chmod 600 ~/sentinel-token
ansible-playbook -i inventory.ini site.yml --ask-vault-pass \
  -e sentinel_token_source=~/sentinel-token
```

この場合 `-K` は不要になります。**使い終わったら消してください。**

#### vault パスワードを毎回入力したくない場合

```bash
echo "vault のパスワード" > ~/.ansible-vault-pass
chmod 600 ~/.ansible-vault-pass
ansible-playbook -i inventory.ini site.yml \
  --vault-password-file ~/.ansible-vault-pass -K
```

**vault の中身を守っているのはこのファイルだけ**になります。
ホームディレクトリが他人から読めない、暗号化されている、
といった前提が置ける場合にのみ使ってください。

### sudo をパスワード無しにするという選択

各ノードで 1 回ずつ sudo できるなら、そちらのほうが恒久的に楽です。
ただし **Ansible は python module を root で実行する**ため、
「sentinel コマンドだけ NOPASSWD」では足りず、実質的に
その運用ユーザーの `NOPASSWD: ALL` が必要になります。

サイトのセキュリティ方針として許容できるかどうかで判断してください。
許容できないなら vault が正解です。

### credential の読み取りについて### credential の読み取りについて

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
| `sentinel_version` | このリポジトリの版 | 取得する release |
| `sentinel_minimum_version` | `0.3.2` | このロールが必要とする最小版 |
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

## バージョンについて

`sentinel_version` の既定値は、**このリポジトリがビルドする版と一致します**
（`tests/ansible_role.rs` が強制します）。古い release を指定すると、
ロールが使う `install --binary` や `doctor --json` の address 系フィールドが
無いため、バイナリを配り終えたあとの `install` で失敗します。

そのため、**バイナリを配置した直後にバージョンを検査**し、
古ければ「command-line の引数エラー」ではなく理由の分かるメッセージで止まります。

## アップグレード

`sentinel_version` を変えて再実行します。設定ファイルと credential は
`sentinel install` が上書きしないため、そのまま残ります。

```bash
ansible-playbook -i inventory.ini site.yml -e sentinel_version=v0.3.1
```

**controller を先に更新してください。** protocol version が同じであれば
混在状態でも動作します。
