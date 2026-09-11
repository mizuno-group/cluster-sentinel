# Cluster Sentinel

計算クラスタの監視・障害検知・原因診断ツール。**バイナリ 1 つ**で動きます。

「ノードが応答しない」ではなく **「なぜ応答しないのか」** を答えることを
目的にしています。原因が違えば、行くべき場所が違うからです。

| 見た目 | Sentinel が言うこと |
| --- | --- |
| node に繋がらない | `HOST_UNREACHABLE` — 本当に落ちている |
| node に繋がらない | `PATH_SPECIFIC_NETWORK_FAILURE` — 経路の一部だけが切れている |
| node に繋がらない | `SSH_SERVICE_FAILURE` — マシンは生きていて SSH だけ死んでいる |
| node に繋がらない | `SENTINEL_AGENT_FAILURE` — agent だけ落ちている |
| node が使えない | `SLURMD_SERVICE_FAILURE` — `slurmd` だけ停止 |
| node が使えない | `SLURM_ONLY_DEGRADATION` — マシンは正常、Slurm 上で drain |
| 何台も同時に不調 | `SHARED_STORAGE_FAILURE` — 共有ストレージ 1 台が原因 |
| fileserver が不調 | `NFS_SERVICE_FAILURE` — マシンは生きていて export だけ死んでいる |

そのために、1 か所からではなく**複数のノードから互いを観測**し、
証言を突き合わせて結論を出します。1 台からしか見えていない不調を
「ホストが落ちた」と言い切ることはしません。

**意図的に存在しない診断結果があります: `POWER_OFF`。**
ネットワークが沈黙していることは、電源が切れている証拠ではありません。
BMC のような別系統の証拠なしに、Sentinel はこれを主張しません。

## はじめに読むもの

**→ [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md)**

はじめて触る人向けの案内です。30 分でクラスタの状態が見えるようになります。

## ドキュメント

### 使う人向け

| 目的 | ドキュメント |
| --- | --- |
| **はじめて触る** | [GETTING_STARTED.md](docs/GETTING_STARTED.md) |
| **コマンドの一覧と使い分け** | [COMMANDS.md](docs/COMMANDS.md) |
| 本番クラスタへ本格導入する | [DEPLOYMENT.md](docs/DEPLOYMENT.md) |
| 日々の運用、障害時の読み方、アップグレード | [OPERATIONS.md](docs/OPERATIONS.md) |
| 設定項目のリファレンス | [CONFIGURATION.md](docs/CONFIGURATION.md) |
| 何をどこまで守るのか | [SECURITY.md](docs/SECURITY.md) |
| 多数のノードへ一括配布する | [deploy/ansible/](deploy/ansible/) |

### 中身を知りたい人向け

| 目的 | ドキュメント |
| --- | --- |
| 設計の考え方とコードの構成 | [ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| 開発環境・テスト・疑似クラスタ | [DEVELOPMENT.md](docs/DEVELOPMENT.md) |
| アーキテクチャ仕様（source of truth） | [SPEC.md](docs/SPEC.md) |
| 実装契約・milestone・テスト要件 | [IMPLEMENTATION.md](docs/IMPLEMENTATION.md) |
| 自明でなかった設計判断の記録 | [adr/](docs/adr/) |
| Docker では検証できない項目 | [VM_VALIDATION.md](docs/VM_VALIDATION.md) |

## インストール

コンパイルは不要です。x86_64 と ARM64 の静的バイナリを配布しています。

```bash
ARCH=$(uname -m) && curl -fsSL -o sentinel "https://github.com/mizuno-group/cluster-sentinel/releases/latest/download/sentinel-${ARCH}-unknown-linux-musl" && chmod +x sentinel && sudo mv sentinel /usr/local/bin/
```

ソースからビルドする場合:

```bash
cargo build --release
```

## コマンド

controller も agent も CLI も、すべて同じバイナリのサブコマンドです。

```bash
sentinel status                   # 今どうなっているか
sentinel diagnose                 # 何が壊れていて、なぜそう言えるのか
sentinel explain                  # 何をどうやって見張っているのか
sentinel audit                    # 動いているはずの検査が動いているか
sentinel entity observations <n>  # その判断の元になった生の観測
sentinel notify test              # 通知先に実際に届くか
```

**一覧と使い分けは [docs/COMMANDS.md](docs/COMMANDS.md)** にまとめてあります。

いずれも `--json` で機械可読出力になります。
`sentinel status` は異常があれば exit code 2 を返すので、
そのまま health check に使えます。

## 互換性について（v1.0 以降）

1.x の間、次は壊しません。

| | 約束 |
| --- | --- |
| `config_version` | `1` のまま。既存の設定ファイルはそのまま動きます |
| `protocol_version` | `1` のまま。**controller と agent のバージョンが混在していても動きます**（アップグレード中は必ずそうなります） |
| `--json` の出力 | フィールドの**追加はあります**が、削除・改名はしません |
| 終了コード | 変えません（[COMMANDS.md](docs/COMMANDS.md#終了コード)） |

人が読む前提のテキスト出力は、読みやすさのために変わることがあります。
**スクリプトからは `--json` を使ってください。**

## 動作確認

Docker の疑似クラスタに対して、受け入れ項目 23 件が自動で走ります。

```bash
cd dev/compose && ./scripts/acceptance
```

## ライセンス

MIT.
