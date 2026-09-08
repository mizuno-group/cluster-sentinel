# ADR 0002 — observation は送信前に spool へ書く

* Status: accepted
* Date: 2026-09-07
* Milestone: M2

## 背景

Agent は observation を controller へ送ります。
controller は到達不能になることがあり、`SPEC.md` §105・§176 は
その間も observation を失わないことを要求しています。

素直な実装は「まず送る、失敗したら spool へ書く」です。
正常時に disk I/O が発生しないため、一見効率的です。

## 決定

順序を逆にします。observation は **常に先に spool へ書き**、
controller が受領を確認した後に spool から削除します。

Spool の読み出しは pop ではなく peek + acknowledge です。

## 帰結

利点:

* **送信と spool 書き込みの間のクラッシュで失われるものが無い。**
  「送る → 失敗したら書く」方式では、送信中にプロセスが死ぬと
  observation は消えます。しかも消えるのは、まさに何かがおかしい瞬間です。
* **acknowledge が来る前に network が切れても失われない。**
  peek 方式なので、受領確認を受け取れなかった場合は再送されます。
  observation ID は agent が採番するため、controller 側の重複挿入は起きません
  （`IMPLEMENTATION.md` §45）。
* 障害時と正常時でコードパスが同一になり、テストしにくい分岐が消えます。

受け入れる欠点:

* **正常時にも disk write が発生する。**
  WAL mode の SQLite への 1 行 INSERT であり、
  `SPEC.md` §122 の資源予算に収まります。
  observation の消失と引き換えにする価値はありません。
* **spool は有限。** 上限（age / rows / bytes）を超えた場合は evict します。
  evict 順序は replay 順序の逆で、**優先度の低い古い行から** 消します。
  失敗の記録が、成功の記録のために捨てられることはありません
  （`IMPLEMENTATION.md` §55）。

## 検討した代替案

**メモリ上の ring buffer + 定期 flush。**
棄却。agent プロセスの死で失われ、しかも
「agent が死ぬような状況」こそ記録が必要な場面です。

**送信を先に、失敗時のみ spool。**
棄却。上記のクラッシュ窓に加え、
正常経路と障害経路が別実装になり、障害経路のテストが手薄になります。
