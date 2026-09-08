# ADR 0003 — Slurm の role は「インストール」ではなく「設定」から判定する

* Status: accepted
* Date: 2026-09-07
* Milestone: M3

## 背景

Capability discovery は当初、実行ファイルの存在で Slurm role を判定していました。

```text
which slurmd    → slurm.compute
which slurmctld → slurm.controller
```

疑似クラスタを実際に動かしたところ、**全 compute node が
`slurm.controller` capability を主張** しました。

原因は単純です。多くのディストリビューションは `slurm-wlm` として
全 daemon を 1 パッケージで配布するため、compute node にも
`slurmctld` バイナリが存在します。

これは軽微な表示上の誤りではありません。
`slurm.controller` は M5 で control plane probe を有効化する条件であり、
このままでは **全 compute node に対して control plane probe が動作** します。

## 決定

Slurm role の判定根拠を、インストール状況ではなく `slurm.conf` の設定内容とします。

* `slurm.controller` — この host が `SlurmctldHost`（または旧 `ControlMachine`）である
* `slurm.compute` — この host が `NodeName` 行に含まれる（hostlist 展開を含む）

いずれも、対応するバイナリが存在することを併せて要求します。

`slurm.conf` が読めない場合に限り、バイナリの存在を弱い根拠として使用します。
根拠が無いよりはましであり、運用者は override できるためです。

この判定ロジックは `src/integrations/slurm/detect.rs` に置きます。
「バイナリがある」と「ここで動く設定になっている」の違いを知っているのは
Slurm integration だけであり、汎用 discovery が知るべきことではありません。

## 帰結

利点:

* **capability が正しくなる。** compute node は control plane を主張しません。
* **capability が安定する。** 設定は daemon が落ちても変わりません。
  これは重要で、daemon の死とともに capability が消える設計では、
  まさにその死を検知すべき probe が停止してしまいます。
* hostlist 展開を再利用でき、`compute[01-03]` のような記述も正しく扱えます。
* FQDN の有無・大文字小文字の差異を吸収します
  （`slurm.conf` が `node01`、host が `node01.example.org` を名乗る場合など）。

受け入れる欠点:

* **`slurm.conf` の読み取りが必要。**
  ローカルパスに限定しているため、network filesystem 上で block する危険はありません
  （`SPEC.md` §75）。
* **`slurm.conf` を持たない compute node では弱い判定へ退化する。**
  設定が配布されていない環境では従来どおりバイナリ判定になります。
  この場合も運用者が `[capabilities]` で明示的に上書きできます。

## 関連する修正

本件の検証中に、より一般的な問題が判明しました。
**一度保存された capability が二度と削除されない** という挙動です。

当初の意図は「provider が一時的に沈黙しただけで capability を消すべきではない」でしたが、
discovery は不在を明示的に報告するため、この保守性は不要に強すぎました。
結果として、誤検出された capability が永続化していました。

`SqliteStore::reconcile_capabilities` を追加し、
**provider 単位で** 主張の取り下げを反映するようにしました。
ある provider が沈黙しても、他の provider の主張は削除されません。
