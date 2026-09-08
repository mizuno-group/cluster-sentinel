# ADR 0001 — natural key から決定的に導出する entity identity

* Status: accepted
* Date: 2026-09-07
* Milestone: M0

## 背景

1 つの entity は複数の provider から独立に発見され得ます。
Slurm discovery、自己登録した agent、静的設定エントリが同じ host を記述することがあり、
これらは 3 つではなく 1 つの entity に収束しなければなりません。

`IMPLEMENTATION.md` §38 は、内部 ID に UUID を用い、merge 用 natural key を
`(environment, entity_type, canonical_name)` とすることを要求しています。

素直な実装は「ランダム UUID + natural key への unique index」であり、
発見のたびに insert-or-lookup を行う方式です。

## 決定

Entity ID は natural key から導出する **UUIDv5** とします。
固定の namespace 定数のもとで、key の 3 フィールドを
`\x1f`（ASCII unit separator）で連結して導出します。

## 帰結

利点:

* DB を参照せずに、任意のコンポーネントが entity ID を計算できます。
  agent は offline でも observation に対象 entity の ID を付与でき、
  spool 再送時に lookup の往復が不要です。
* Merge が「既知の primary key への upsert」になり、
  並行する provider 間の lookup-then-insert race が消滅します。
* Test fixture が安定した ID を参照でき、golden test が導出を固定します。

受け入れる欠点:

* **entity の rename は ID を変える。**
  `node01` から `compute01` へ改名された host は新しい entity となり、履歴は引き継がれません。
  rename は稀かつ意図的な行為であり、後から運用者主導の明示的 merge を追加できるため、
  許容します。棄却した代替案が招く「IP 変更による暗黙の identity drift」は
  はるかに深刻かつ高頻度です。
* **namespace 定数と separator は永久に凍結される。**
  いずれかを変更すると既存の全行が孤児化します。
  この旨は `src/entity/id.rs` にコメントとして明記され、
  変更しようとすれば golden test が明確に失敗します。

separator は重要です。これが無いと
`("a", host, "b-c")` と `("a-b", host, "c")` が衝突します。
まさにこれを検証するテストがあります。

## 検討した代替案

**ランダム UUID + unique index。**
棄却。controller が到達不能な状況でも動作しなければならない経路に
DB 往復を持ち込むためです。まさにその状況こそ Sentinel が機能し続けるべき局面です。

**natural key を primary key にする。**
棄却。schema 中のすべての foreign key に、可変で運用者向けの文字列が入り込みます。
また、将来の「履歴を保持した rename」機能を、未実装どころか実装不可能にします。
