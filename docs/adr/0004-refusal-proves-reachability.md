# ADR 0004 — TCP の refusal は「到達可能」の証拠として扱う

* Status: accepted
* Date: 2026-09-07
* Milestone: M4

## 背景

汎用の到達性 probe（`network.tcp`）は、host へ TCP 接続を試みます。
接続先ポートには、その host で確実に動いているサービスを選ぶ必要があり、
当初は SSH（22番）を使っていました。

疑似クラスタで `sshd` のみを停止したところ、次の結果になりました。

```text
ssh        UNAVAILABLE
network    UNAVAILABLE     ← 誤り
```

`sshd` を止めただけで network component まで異常になっています。

これは表示上の不正確さでは済みません。
`HOST_UNREACHABLE` と service 障害を区別する診断は
「network が到達可能かどうか」を根拠にします。
service を 1 つ止めるだけで network が unreachable になるなら、
**本システムが存在する理由そのものである区別が成立しません。**

## 決定

到達性 probe においては、**connection refused を成功として扱います。**

refusal はパケットです。その address にいる何かが接続を受け取り、応答しています。
死んだ host は RST を返しません。

一方、特定サービスの生存確認を目的とする probe では、
同じ refusal を失敗として扱います。サービスは listen しているべきだからです。

同じパケットが、問いによって異なる意味を持ちます。

* 「この host に到達できるか」→ refusal は **Yes**
* 「このサービスは動いているか」→ refusal は **No**

実装では `TcpProbe::reachability()` と `TcpProbe::new()` で区別します。

## 帰結

利点:

* service 障害と host 到達不能が独立に観測できます。
  これは `SPEC.md` §170 / §171 / §175 の受け入れ条件そのものです。
* refusal という **積極的な証拠** を捨てずに済みます。
  応答があったという事実は、沈黙より多くを語ります。
* observation の payload には `host_responded` を常に記録するため、
  診断エンジンは「接続できた」と「到達はできた」を後段でも区別できます。

受け入れる欠点:

* **到達性 probe は、対象ポートで何が動いているかを問いません。**
  ポートが filter されて RST が返る構成では、
  実際には通信できない経路を到達可能と判定し得ます。
  ただしこの場合も、SSH probe と agent probe が独立に失敗を報告するため、
  異常が見逃されることはありません。
* 到達性の判定に使うポート選択は依然として設定事項です。
  現在は SSH ポートを既定としていますが、
  refusal を成功として扱うため、そこで何が動いているかは重要ではなくなりました。

## 検討した代替案

**Sentinel agent のポート（7444）を到達性判定に使う。**
棄却。agent が停止した場合に network 側も異常となり、
逆向きの同じ問題が生じます。

**複数ポートを試し、すべて失敗した場合のみ到達不能とする。**
より頑健ですが、host あたりの接続数が増えます。
refusal を証拠として扱うだけで同じ結論が得られるため、現時点では不要と判断しました。
将来 filter 環境で必要になれば追加できます。

**ICMP を使う。**
棄却。frequently filtered であり、raw socket に特権が必要で、
`SPEC.md` §59 も ICMP を optional としています。
