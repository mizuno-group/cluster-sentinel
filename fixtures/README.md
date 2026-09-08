# Fixtures

実コマンド出力を保存し、parser テストの入力とするディレクトリです。

**core は fixtures を参照しません**（`docs/IMPLEMENTATION.md` §79）。
参照するのはテストのみです。

`fixtures/slurm/` の内容は `scontrol` の実出力形式に忠実ですが、
host 名は実在しないもの（`compute-a`、`ctl-a` 等）へ置き換えてあります。
これは実 deployment のトポロジがテストデータ経由で実装へ染み出すのを防ぐためです。

| ファイル | 由来 | 含まれる状況 |
| --- | --- | --- |
| `slurm/show_nodes.txt` | `scontrol show nodes -o` | IDLE / MIXED / IDLE+DRAIN / DOWN*、GPU あり・なし、空白を含む `OS=`、free-form な `Reason=` |
| `slurm/show_partitions.txt` | `scontrol show partitions -o` | 圧縮 hostlist、default partition、複数 partition |
| `slurm/ping_up.txt` | `scontrol ping` | controller 1 台構成 |
| `slurm/ping_ha.txt` | `scontrol ping` | primary/backup 構成、backup が DOWN |
| `slurm/ping_down.txt` | `scontrol ping` | controller へ到達できない場合 |
