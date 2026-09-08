# Cluster Sentinel
## 汎用クラスタインフラ監視・障害検知・インシデント診断基盤

**Version:** 0.3  
**Status:** Architecture Specification  
**Initial production target:** `example_cluster` および関連インフラ  
**Primary implementation language:** Rust  
**Deployment model:** Single executable binary per architecture  
**Initial platform:** Linux / systemd / Slurm / NFS  
**Design goal:** 現在のクラスタ構成に実用的に適合しつつ、将来的な計算・ストレージ・ネットワーク・scheduler構成変更に対して、core architectureを変更せず設定・integration・probe追加のみで追従できる監視基盤を構築する。

---

# 1. 目的

Cluster Sentinelは、研究用計算クラスタを構成する各種インフラの状態を複数地点から観測し、

```text
異常検知
    ↓
障害レイヤー切り分け
    ↓
依存関係解析
    ↓
インシデント相関
    ↓
証拠保存
    ↓
管理者通知
```

までを行う分散監視・診断システムである。

主な対象障害:

- 計算ノードが停止した
- OSは起動しているがSSH不能
- SSHは可能だが `slurmd` が停止
- Hostは正常だがSlurm上のみ `DRAIN`
- Slurm controllerのみ異常
- NFS serverが停止
- NFSは応答するがI/Oが極端に遅い
- 共有ストレージ障害で複数compute nodeが連鎖的に異常化
- GPU障害
- local storage / NVMe異常
- network path固有障害
- monitoring controller自身の障害
- reboot後に原因情報が失われる

---

# 2. v0.3における最重要変更

v0.2まではHostを中心概念としていた。

v0.3では、Hostを含むより一般的な、

```text
ManagedEntity
```

をcore modelとする。

これにより、

```text
Host
Service
Storage
Scheduler
Network Endpoint
External Dependency
```

等を同一監視モデルへ統合する。

---

# 3. Architecture Principle

Cluster Sentinel coreは、

```text
Slurm monitor
NFS monitor
GPU monitor
```

ではない。

coreが扱うのは以下だけである。

```text
Entity
Capability
Dependency
Probe
Observation
State
Diagnosis
Incident
```

Slurm、NFS、GPU等はすべてintegrationとして実装する。

---

# 4. Top-Level Model

```text
Environment
│
├── Cluster
│
├── ManagedEntity
│   ├── Host
│   ├── Service
│   ├── Storage
│   ├── Scheduler
│   ├── NetworkEndpoint
│   └── ExternalDependency
│
├── Capability
├── DependencyGraph
├── Probe
├── Observation
├── Diagnosis
└── Incident
```

---

# 5. Environment

最上位概念を、

```text
Environment
```

とする。

現在は1環境のみでもよい。

例:

```text
example-lab
```

将来的に、

```text
example-lab
├── production-cluster
├── test-cluster
└── workstation-group
```

のような構成も扱える。

---

# 6. Cluster

ClusterはManagedEntityの集合を論理的にグループ化する。

現在:

```text
Cluster:
example_cluster
```

ただしClusterとSlurm Clusterを同義にしてはならない。

将来的に、

```text
Slurm cluster
storage cluster
monitoring domain
```

が異なる場合にも対応する。

---

# 7. ManagedEntity

Sentinelが状態を持つ監視対象。

基本属性:

```text
entity_id
entity_type
display_name
environment_id
cluster_id optional
labels
metadata
discovery_sources
```

---

# 8. Entity Types

初期実装で定義する。

```text
Host
Service
Storage
Scheduler
ExternalDependency
```

NetworkDevice等は将来追加可能。

---

# 9. Host Entity

物理または仮想Linux machine。

例:

```text
head01
node01
node08
node09
node02
...
filesrv01
filesrv02
```

Hostは、

```text
IP
hostname
CPU
RAM
GPU
```

等を属性として持つ。

---

# 10. Service Entity

Host上で動くサービスを独立entityとして扱う。

例:

```text
slurmctld@head01
slurmdbd@head01
slurmd@node09
sshd@node09
nfs-server@filesrv01
```

これにより、

```text
Host healthy
Service failed
```

を自然に表現できる。

---

# 11. Storage Entity

StorageをHostとは独立して表現可能にする。

現在:

```text
filesrv01-backed-storage
filesrv02-backed-storage
```

将来:

```text
Ceph cluster
Lustre filesystem
BeeGFS filesystem
NFS VIP
ZFS pool
```

等にも対応可能。

---

# 12. Scheduler Entity

Scheduler/control planeも独立entityとする。

現在:

```text
Slurm scheduler: example_cluster
```

Host `head01` とSchedulerそのものを区別する。

例:

```text
Host(head01)
    ↓ hosts
Service(slurmctld)
    ↓ provides
Scheduler(example_cluster)
```

---

# 13. ExternalDependency

Agentを直接インストールできない対象もdependencyとして表現する。

例:

```text
DNS server
gateway
switch
UPS
external authentication server
BMC endpoint
```

---

# 14. Roles

Host等にhuman-readableなrole labelを設定できる。

例:

```text
controller
compute
fileserver
observer
login
storage
```

ただしroleは**ロジック制御に使用してはならない**。

---

# 15. Role使用原則

禁止:

```text
if role == "fileserver":
    run_nfs_probe()
```

推奨:

```text
if capability == "nfs_server":
    enable_nfs_server_probe()
```

Roleは、

```text
UI grouping
operator understanding
default configuration hints
```

にのみ使用する。

---

# 16. Capability Model

各Entityは0個以上のcapabilityを持つ。

例:

```text
host.metrics
network.icmp
network.tcp
ssh.server
systemd
slurm.controller
slurm.compute
gpu.nvidia
storage.local
storage.nfs.client
storage.nfs.server
storage.zfs
storage.smart
journal
observer.peer
```

---

# 17. Capability Composition

例えば `filesrv01`:

```text
Host(filesrv01)

Capabilities:
host.metrics
network.tcp
ssh.server
systemd
storage.local
storage.nfs.server
storage.zfs
journal
observer.peer
```

`node09`:

```text
Host(node09)

Capabilities:
host.metrics
network.tcp
ssh.server
systemd
slurm.compute
gpu.nvidia
storage.nfs.client
journal
observer.peer
```

---

# 18. Capability Discovery

Agentはruntimeでcapability候補を検出する。

例:

```text
slurmd exists
→ slurm.compute

slurmctld exists
→ slurm.controller

nvidia-smi usable
→ gpu.nvidia

NFS mount exists
→ storage.nfs.client

nfs-server exists
→ storage.nfs.server

zpool available and pool exists
→ storage.zfs
```

---

# 19. Capability Override

Auto detection結果は管理者設定で、

```text
enable
disable
force
```

できる。

例:

```toml
[capabilities]
"storage.nfs.server" = "force"
"storage.smart" = "disable"
```

---

# 20. Current Environment

Initial environment:

```text
example-lab
```

Initial cluster:

```text
example_cluster
```

---

# 21. Current Hosts

Slurm controller:

```text
head01
```

Slurm compute:

```text
node02
node03
node04
node05
node06
node07

node01

node08
node09

node10
node11

node12
node13
```

Non-Slurm infrastructure:

```text
filesrv01
filesrv02
```

初期監視対象は計16 hosts。

---

# 22. Current Slurm Configuration Characteristics

```text
SlurmctldHost=head01

SlurmctldPort=6817
SlurmdPort=6818

ReturnToService=1

AccountingStorageHost=head01

SelectType=select/cons_tres
SelectTypeParameters=CR_Core_Memory

GresTypes=gpu
```

SentinelはSlurm configurationを変更しない。

---

# 23. Current Storage Domains

filesrv01:

```text
node02
node03
node04
node13
node11
```

filesrv02:

```text
node05
node06
node07
node12
node10
```

この関係はcurrent deployment dataとして扱い、core architectureには埋め込まない。

---

# 24. DependencyGraph

すべての依存関係を汎用有向グラフとして表現する。

Edge:

```text
source_entity
target_entity
dependency_type
criticality
metadata
discovery_source
```

---

# 25. Dependency Direction

原則として、

```text
A depends_on B
```

を、

```text
A → B
```

として扱う。

例:

```text
node02
    → filesrv01-backed-storage
```

---

# 26. Dependency Type

初期:

```text
depends_on
hosted_on
provides
uses_storage
uses_scheduler
network_reaches
observes
```

Integration specific metadataを付与可能。

---

# 27. Arbitrary DAG

DependencyGraphはNFS専用treeではない。

将来:

```text
compute01
   ↓
CephFS
   ↓
storage-network
   ↓
switch01
```

や、

```text
compute01
   ↓
Slurm Scheduler
   ↓
slurmctld VIP
   ↓
controller01
controller02
```

を表現可能とする。

---

# 28. Cycles

Operational dependencyにはcycleが生じる場合もあるため、storage layer上はgeneral directed graphを許可する。

ただしroot-cause traversal時にはvisited setを必須とし、無限再帰を禁止する。

---

# 29. Dynamic Grouping

「filesrv01-side」等のgroupをcoreに固定定義しない。

DependencyGraphから、

```text
same upstream storage dependency
```

を共有するentity群として動的導出する。

---

# 30. Inventory Architecture

Inventoryは複数providerから生成する。

```text
Inventory Provider
├── Slurm
├── Agent Registration
├── Static Configuration
└── Future Providers
```

---

# 31. Slurm Inventory Provider

以下からcompute node等を発見。

```bash
scontrol show nodes -o
scontrol show partitions -o
```

Slurm上から消えたhostを即Sentinel inventoryから削除してはならない。

---

# 32. Agent Registration Provider

Sentinel agentがcontrollerへ自己登録する。

これにより、

```text
filesrv01
filesrv02
login nodes
other hosts
```

をSlurmとは独立して発見できる。

---

# 33. Static Inventory Provider

Agentをまだ配布していないhostもconfigで登録可能。

```toml
[[entities.host]]
name = "filesrv01"

[[entities.host]]
name = "filesrv02"
```

---

# 34. Inventory Merge

同一entityが複数providerから発見された場合、

```text
Slurm
Agent
Static config
```

をmergeする。

Discovery sourceは保存する。

---

# 35. Entity Deletion

Entityが一時的にdiscoveryされなくなっても自動削除しない。

状態:

```text
ACTIVE
STALE
REMOVED
```

等を持つ。

インフラ変更による一時的不整合と障害を区別する。

---

# 36. Host Identity

Host logical identity:

```text
environment + stable host ID
```

を使用する。

通常hostnameをinitial identity keyとして利用する。

---

# 37. IP非依存

IPをhost primary keyにしてはならない。

1 hostは、

```text
IPv4
IPv6
management network
storage network
InfiniBand
```

等、複数addressを持てる。

---

# 38. Slurm NodeNameとHostnameの分離

将来、

```text
Slurm NodeName != hostname
```

でも動作すること。

Mapping:

```text
Host Entity
    ↕
Slurm Node Identity
```

として別途保持する。

---

# 39. Runtime Discovery

Agentが取得:

```text
hostname
FQDN
boot ID
IP addresses
interfaces
OS
kernel
CPU
RAM
GPU
mounts
services
storage
Slurm
NFS
ZFS
```

---

# 40. Single Binary

原則:

```text
sentinel
```

1 executable。

Subcommands:

```bash
sentinel controller
sentinel agent
sentinel status
sentinel entity
sentinel incident
sentinel doctor
sentinel config
```

---

# 41. Architecture-specific Binary

CPU architectureが異なる場合のみ、

```text
sentinel-linux-x86_64
sentinel-linux-aarch64
```

等を生成する。

マシン固有buildは禁止。

---

# 42. Controller Location

Controllerを `head01` にハードコードしてはならない。

現在:

```text
head01
```

はdeployment configuration上のcontrollerに過ぎない。

---

# 43. Multiple Sentinel Controllers

v1でHA controllerを実装する必要はない。

ただしdata model上、

```text
1 environment = exactly 1 controller
```

という制約を置いてはならない。

将来的な、

```text
primary controller
standby controller
distributed collectors
```

を許容する。

---

# 44. Agent

全Linux hostへ同じagent binaryを配布可能とする。

Agent責務:

```text
self observation
capability discovery
probe execution
peer observation
local buffering
event collection
controller communication
```

---

# 45. Observer Capability

`observer.peer` capabilityを持つhostは他entityへのremote probeを実施できる。

現在推奨:

```text
head01
filesrv01
filesrv02
compute nodes
```

---

# 46. Peer Monitoring

単一中央監視だけに依存しない。

各重要hostを複数observerから観測する。

---

# 47. Peer Assignment

Controllerが動的にassignmentする。

Default candidate:

```text
peer degree = 3
```

---

# 48. Topology-aware Assignment

可能なら、

```text
same dependency domain
different dependency domain
independent infrastructure observer
```

からそれぞれ観測点を選ぶ。

---

# 49. Example

node02:

```text
node03   → same filesrv01 domain
node05   → different storage domain
filesrv02  → infrastructure observer
```

---

# 50. Observation Quorum

単一probe failureだけでhost failureと確定しない。

例:

```text
head01 → node09 FAIL
node05 → node09 OK
filesrv01 → node09 OK
```

なら、

```text
PATH_SPECIFIC_NETWORK_FAILURE
```

候補。

---

# 51. Host Unreachable

独立observer複数から失敗:

```text
head01 → node09 FAIL
node05 → node09 FAIL
filesrv01 → node09 FAIL
```

なら、

```text
HOST_UNREACHABLE
```

confidence HIGH。

---

# 52. Power State

通常ネットワークprobeから、

```text
POWER_OFF
```

を断定してはならない。

確定可能なのは、

```text
HOST_UNREACHABLE
```

まで。

---

# 53. Out-of-band Future Integration

将来:

```text
IPMI
Redfish
BMC
smart PDU
UPS
```

をintegration追加した場合、

```text
POWER_OFF_CONFIRMED
POWER_ON_BUT_HOST_UNRESPONSIVE
```

等を判別可能にする。

---

# 54. Probe Architecture

Probeをcoreから疎結合化する。

概念interface:

```text
Probe
├── probe_id
├── required_capability
├── target_entity_type
├── interval
├── timeout
├── execution_mode
└── collect()
```

---

# 55. Probe Result

共通schema:

```text
probe_id
entity_id
timestamp
status
latency
structured_data
evidence
error
```

Status:

```text
OK
DEGRADED
FAILED
TIMEOUT
STUCK
UNSUPPORTED
NOT_APPLICABLE
```

---

# 56. Probe Plugins / Integrations

初期:

```text
Host
Network
SSH
Systemd
Slurm
GPU
NFS
Filesystem
Clock
Journal
```

将来:

```text
ZFS
SMART
NVMe
Ceph
Lustre
BeeGFS
InfiniBand
IPMI
Redfish
UPS
Prometheus import
```

---

# 57. Probe追加時の原則

新しいstorage技術導入時に、

```text
core state engine
incident engine
database core
```

を書き換えなくて済むこと。

必要なのは原則、

```text
new capability
new probe
new diagnosis rule
```

のみ。

---

# 58. Host Probe

取得:

```text
boot ID
uptime
load
memory
CPU pressure
memory pressure
```

---

# 59. Network Probe

```text
name resolution
ICMP optional
TCP
Sentinel RPC
service-specific TCP
```

---

# 60. SSH Probe

```text
TCP/22
SSH protocol response
local sshd service state
```

認証を必須としない。

---

# 61. Sentinel RPC

Agent health endpoint。

返却:

```text
entity identity
boot ID
agent version
capabilities
timestamp
health summary
```

---

# 62. Service Monitoring

Service entityを、

```text
systemd
process
port
protocol
application-level probe
```

の組み合わせで観測可能にする。

---

# 63. Slurm Integration

Slurmは独立integrationとして実装。

MVPでは、

```bash
scontrol ping
scontrol show nodes -o
scontrol show partitions -o
squeue
```

等を利用する。

---

# 64. libslurm依存

MVPではlibslurmへcompile-time dependencyしない。

理由:

```text
Slurm version compatibility
binary portability
deployment simplicity
```

---

# 65. Slurm Scheduler Entity

例:

```text
Scheduler:
example_cluster
```

状態:

```text
AVAILABLE
DEGRADED
UNAVAILABLE
```

---

# 66. Slurm Node Integration

HostとSlurm Node identityを関連付ける。

保存:

```text
NodeName
NodeAddr
NodeHostName
State
StateFlags
Reason
ReasonTime
Partitions
CfgTRES
AllocTRES
```

---

# 67. Slurm-only Failure

```text
Host       HEALTHY
Agent      HEALTHY
SSH        HEALTHY
slurmd     HEALTHY
Storage    HEALTHY
Slurm      DRAIN
```

↓

```text
SCHEDULER_DEGRADED
```

---

# 68. slurmd Failure

```text
Host       HEALTHY
Agent      HEALTHY
SSH        HEALTHY
slurmd     FAILED
Slurm      abnormal
```

↓

```text
SLURMD_SERVICE_FAILURE
```

---

# 69. Slurm Hardware Registration Validation

Agent-observed:

```text
CPU
RAM
GPU
```

とSlurm configurationを比較。

Mismatch:

```text
RESOURCE_CONFIGURATION_MISMATCH
```

---

# 70. ReturnToService

現在:

```text
ReturnToService=1
```

Sentinelは、

```text
DOWN
→ host recovery
→ slurmd registration
→ Slurm automatic return
```

というtransitionを記録する。

---

# 71. Storage Integration

StorageはNFSに限定しない。

Generic abstraction:

```text
StorageEntity
```

を持つ。

---

# 72. Current NFS Model

現在:

```text
filesrv01 host
    ↓ provides
NFS service
    ↓ provides
storage entity
    ↓ used by
compute nodes
```

---

# 73. NFS Client Probe

```text
mount existence
mount source
server reachability
read-only state
active probe latency
```

---

# 74. NFS Server Probe

```text
service state
port 2049
export state
filesystem state
NFS health
```

---

# 75. NFS Safety

NFS障害時のblocking syscallに注意。

禁止:

```text
unlimited df
unlimited stat
unlimited ls
```

---

# 76. Outstanding Probe Limit

各NFS mount:

```text
max active filesystem probe = 1
```

前probeがstuckしていれば新規filesystem probeを起動しない。

---

# 77. NFS Status

```text
NFS_OK
NFS_SLOW
NFS_TIMEOUT
NFS_STUCK
```

---

# 78. Storage Technology Replacement

将来NFSからCephFSへ変更しても、

```text
NFS Capability OFF
Ceph Capability ON
Ceph Probe added
Dependency edges updated
```

のみで対応可能なこと。

---

# 79. ZFS Integration

Optional。

```text
pool health
device health
scrub state
error counters
capacity
```

---

# 80. SMART/NVMe

Optional。

```text
temperature
media errors
critical warning
unsafe shutdown
NVMe error
```

---

# 81. GPU Integration

Capability:

```text
gpu.nvidia
```

MVPは `nvidia-smi` 利用可。

取得:

```text
GPU count
UUID
model
temperature
memory
utilization
power
```

---

# 82. GPU Configuration Mismatch

Slurm GRES expected GPU countとobserved countを比較。

---

# 83. Kernel Event Integration

Raw kernel/journal eventとして保存。

例:

```text
OOM
NFS server not responding
NVMe timeout
I/O error
GPU Xid
hung task
network down
```

これ自体を診断結果と等価にしない。

---

# 84. Observation Model

Probeが返す事実を、

```text
Observation
```

としてimmutable保存する。

Diagnosisとは分離。

---

# 85. Observation Example

```text
entity:
filesrv01

probe:
nfs.service

result:
FAILED

timestamp:
10:35:03
```

---

# 86. State Model

Observation群からEntity component stateを導出する。

例:

```text
HostState
ServiceState
StorageState
SchedulerState
```

---

# 87. Entity Overall State

共通summary:

```text
HEALTHY
DEGRADED
UNAVAILABLE
MAINTENANCE
UNKNOWN
```

詳細classificationは別フィールド。

---

# 88. Classification

例:

```text
SSH_DEGRADED
SCHEDULER_DEGRADED
STORAGE_DEGRADED
HOST_UNREACHABLE
SERVICE_FAILURE
```

---

# 89. State Transition

状態の変化をevent化。

```text
HEALTHY
↓
DEGRADED
↓
UNAVAILABLE
```

---

# 90. Debounce / Hysteresis

default candidate:

```text
warning:
2 failures

critical:
3 failures

recovery:
2 successes
```

probeごとに変更可能。

---

# 91. Diagnosis Engine

ObservationとDependencyGraphから、

```text
DiagnosisCandidate
```

を生成する。

---

# 92. Diagnosis Structure

```text
diagnosis_type
affected_entities
suspected_root_entities
confidence
evidence_refs
reasoning_rules
timestamp
```

---

# 93. Confidence

```text
LOW
MEDIUM
HIGH
CONFIRMED
```

---

# 94. Deterministic Diagnosis

Core diagnosisはrule-based / graph-based。

LLMを必須にしない。

---

# 95. Shared Dependency Correlation

例:

```text
node02
node03
node04
node13
node11
```

でstorage障害が同時発生。

Graph:

```text
all → filesrv01-backed-storage
```

filesrv01自身にも異常あり。

↓

```text
SHARED_STORAGE_FAILURE
confidence HIGH
```

---

# 96. Client-local Storage Failure

filesrv01正常。

node02のみ異常。

↓

```text
LOCAL_STORAGE_CLIENT_FAILURE
```

候補。

---

# 97. Scheduler-wide Incident

複数compute nodeで同時にSlurm state取得不能だが、

```text
Host
SSH
Agent
```

は正常。

`slurmctld` service異常。

↓

```text
SLURM_CONTROL_PLANE_FAILURE
```

---

# 98. Incident Engine

複数event/diagnosisを、

```text
Incident
```

へまとめる。

---

# 99. Incident Structure

```text
incident_id
status
severity
start_time
end_time
affected_entities
suspected_root_entities
diagnoses
evidence
timeline
```

---

# 100. Severity

```text
INFO
WARNING
CRITICAL
```

dependency fan-out等を考慮可能。

---

# 101. Dependency-aware Severity

filesrv01障害:

```text
5 compute dependents
```

単独compute node障害より高severityにできる。

---

# 102. Incident Correlation Factors

```text
temporal proximity
shared dependencies
same probe failures
same service
same scheduler
same network path
same storage
```

---

# 103. Evidence Preservation

Incident発生時、

```text
raw observations
peer observations
entity state
dependency snapshot
Slurm state
service state
storage state
kernel events
```

を保存。

---

# 104. Ring Buffer

Agent/controllerは直近、

```text
30 minutes
```

程度の高頻度observationsを保持。

configurable。

---

# 105. Local Spool

Controller unreachableでも、

```text
self observations
peer observations
important events
```

をローカル保存。

---

# 106. Reconnection

Controller復旧後、spool dataをtimestamp順に再送。

Duplicate ID等によりidempotent ingestionを保証する。

---

# 107. Reboot Detection

Linux boot ID変更:

```text
HOST_REBOOTED
```

として保存。

---

# 108. Clock Skew

Distributed timeline整合性のため、

```text
CLOCK_SKEW
```

を監視。

---

# 109. Controller Failure

Controller自身の死活をpeerから監視。

---

# 110. Fallback Alerting

Current recommendation:

```text
filesrv01
filesrv02
```

にfallback notifier capabilityを持たせる。

---

# 111. Notification Deduplication

Controllerとfallback observerから同じincidentが二重通知されないよう、

```text
incident fingerprint
notification lease
deduplication key
```

等を使用する。

---

# 112. Notification Backends

```text
ntfy
Gotify
Slack
Discord
Email
generic webhook
```

provider abstractionで実装。

---

# 113. Automatic Remediation

v1では禁止。

```text
reboot
systemctl restart
scontrol resume
mount/remount
```

を自動実行しない。

---

# 114. Recommended Actions

診断結果からread-onlyな推奨調査commandを提示可能。

例:

```text
systemctl status slurmd
journalctl -u slurmd
scontrol show node node09
```

---

# 115. Security

Agent/controller間通信は認証必須。

最終推奨:

```text
mTLS
```

---

# 116. Agent Remote Execution禁止

Sentinel RPCから任意shell commandを実行可能にしてはならない。

Probeは事前定義済み処理だけを実行する。

---

# 117. Privilege

可能な限り、

```text
sentinel
```

専用user。

read-only monitoringを基本とする。

---

# 118. Optional Privileged Probes

```text
SMART
NVMe
some journal access
BMC
```

等のみ明示的権限追加。

---

# 119. Probe Sandbox

Probe failure/panicがagent全体を落とさないよう隔離する。

---

# 120. External Commands

以下を実行する場合:

```text
systemctl
journalctl
scontrol
squeue
nvidia-smi
zpool
smartctl
```

必ずtimeoutを設定する。

---

# 121. NFS Exception

Kernel D-stateではprocess kill不能の可能性があるため、通常のcommand timeoutとは別のstuck handlingを実装する。

---

# 122. Resource Budget

Agent目標:

```text
idle CPU << 1 core
RAM <100 MB desirable
low disk I/O
low network traffic
```

---

# 123. Polling Defaults

候補:

```text
Sentinel heartbeat       5 s
peer RPC                 5 s
network                  5 s
SSH                     15 s
Slurm                    5 s
service                 10 s
GPU                     15 s
filesystem              30 s
NFS active              30 s
ZFS                     60 s
inventory              300 s
```

jitter必須。

---

# 124. Database

MVP:

```text
SQLite
```

WAL mode。

---

# 125. Storage Abstraction

Repository interfaceを設け、

将来:

```text
PostgreSQL
```

へ移行可能。

---

# 126. Core Database Entities

```text
environments
clusters

entities
entity_labels
entity_capabilities
entity_addresses

dependencies

agent_instances
agent_sessions

probes
observations

entity_states
state_transitions

diagnoses

incidents
incident_entities
incident_evidence

notifications
acknowledgements
maintenance_windows
```

---

# 127. Schema Versioning

Configに必須:

```toml
config_version = 1
```

Databaseにもschema migration versionを持つ。

---

# 128. Config Migration

将来config format変更時、

```bash
sentinel config migrate
```

等でmigration可能にする。

---

# 129. Config Philosophy

現在のcluster topologyをsource codeへ入れない。

Deployment-specific dataは、

```text
runtime discovery
agent registration
configuration
```

のみ。

---

# 130. Minimal Agent Configuration

例:

```toml
config_version = 1
environment = "example-lab"

[controller]
address = "head01:7443"
```

Roleすらauto/manual hybridにできる。

---

# 131. Controller Configuration

```toml
config_version = 1

environment = "example-lab"

[controller]
listen = "0.0.0.0:7443"

[discovery.slurm]
enabled = true

[database]
path = "/var/lib/sentinel/sentinel.db"

[peer_monitoring]
degree = 3
```

---

# 132. Explicit Entity Config

```toml
[[entities]]
type = "host"
name = "filesrv01"

[[entities]]
type = "host"
name = "filesrv02"
```

---

# 133. Labels

柔軟なmetadataとしてlabelsを利用可能。

例:

```text
location=entrance-side
location=professor-room
rack=rack01
storage_domain=legacy-o
```

ただし診断coreがspecific label名に依存してはならない。

---

# 134. Installation

```bash
sudo install -m 0755 sentinel /usr/local/bin/sentinel
```

Agent:

```bash
sudo sentinel install agent --controller head01:7443
```

Controller:

```bash
sudo sentinel install controller
```

---

# 135. systemd

必要なservice/unitをinstallerが生成可能。

---

# 136. CLI

```bash
sentinel status

sentinel entity list
sentinel entity show <id>

sentinel dependency list
sentinel dependency graph

sentinel incident list
sentinel incident show <id>

sentinel peers

sentinel doctor

sentinel config check
sentinel config migrate

sentinel version
```

---

# 137. `sentinel status`

例:

```text
ENVIRONMENT: example-lab

Infrastructure
────────────────────────────
head01       HEALTHY
filesrv01    HEALTHY
filesrv02    DEGRADED

Compute
────────────────────────────
node02     HEALTHY
node03     HEALTHY
...
node09      DEGRADED
```

---

# 138. Entity Detail

```text
Entity:
Host(node09)

Capabilities:
host.metrics
ssh.server
slurm.compute
gpu.nvidia
storage.nfs.client

Overall:
DEGRADED

Classification:
SCHEDULER_DEGRADED
```

---

# 139. Dependency View

```text
node02
   │
   ├── uses_scheduler → example_cluster
   │
   └── uses_storage → filesrv01-storage
                          │
                          └── provided_by → filesrv01
```

---

# 140. Web UI

MVP後。

主要画面:

```text
Environment overview
Entity list
Entity details
Dependency graph
Incident timeline
Historical state
```

---

# 141. UI Entity Type Independence

Frontendでも、

```text
if hostname == filesrv01
```

のようなspecial caseは禁止。

Entity metadata/capabilityでrenderする。

---

# 142. Current Production Deployment

```text
head01
  Host
  controller-related capabilities
  observer

filesrv01
  Host
  NFS/storage capabilities
  observer

filesrv02
  Host
  NFS/storage capabilities
  observer

13 Slurm compute nodes
  Host
  Slurm compute capability
  storage client capability
  optional GPU capability
```

---

# 143. Current Initial Dependency Graph

概念:

```text
node02 ─┐
node03 ─┤
node04 ─┼──→ filesrv01-storage → filesrv01
node13   ─┤
node11  ─┘


node05 ─┐
node06 ─┤
node07 ─┼──→ filesrv02-storage → filesrv02
node12─┤
node10  ─┘
```

node01/node08/node09のstorage dependencyはruntime discoveryまたはconfigから登録する。

---

# 144. Future Scenario: Slurm Controller Migration

現在:

```text
head01
```

将来:

```text
control01
```

に移行。

必要変更:

```text
deployment config
Slurm discovery result
dependency edges
```

のみ。

Core code変更不要。

---

# 145. Future Scenario: Slurm HA

```text
control01
control02
    ↓
Slurm scheduler
```

Service/Scheduler entityとdependency edge追加で対応する。

Core architecture変更不要。

---

# 146. Future Scenario: NFS → CephFS

旧:

```text
NFS storage
```

新:

```text
Ceph storage
```

変更:

```text
disable NFS capabilities
enable Ceph capability
add Ceph probes
update dependencies
```

Incident engine等は変更不要。

---

# 147. Future Scenario: New GPU Nodes

20台追加されても、

```text
Slurm discovery
Agent registration
Capability discovery
```

でinventoryへ追加。

Host固有source code変更不要。

---

# 148. Future Scenario: Login Nodes

```text
login01
login02
```

追加。

Capabilities:

```text
host
network
ssh
observer
```

必要なprobeだけ実行。

---

# 149. Future Scenario: InfiniBand

Capability:

```text
network.infiniband
```

Probe:

```text
IB link
port state
error counters
```

を追加。

Core変更不要。

---

# 150. Future Scenario: BMC

ExternalDependencyまたはEndpoint Entityとして、

```text
BMC
```

を追加。

Redfish integration追加のみ。

---

# 151. Future Scenario: Multi-cluster

```text
Environment: example-lab

Cluster:
example_cluster
cluster2
test_cluster
```

を同一controller/databaseで管理可能なdata modelとする。

v1でUI対応まで必須ではない。

---

# 152. Test Strategy

```text
Unit tests
Mock probes
Integration tests
VM-based tests
Production staged rollout
```

---

# 153. Mock Infrastructure

任意entity/probeを、

```text
healthy
degraded
timeout
failed
stuck
```

へ変更可能なsimulation backendを用意する。

---

# 154. Required Simulations

```text
SSH failure
slurmd failure
Slurm DRAIN
controller failure
host unreachable
NFS server failure
NFS client-only failure
multiple dependent node failures
GPU missing
clock skew
agent restart
host reboot
```

---

# 155. Compatibility Tests

Unknown Slurm fieldsやversion差でparser全体が失敗しないこと。

---

# 156. Rollout Phase 1

Controller + Slurm discovery。

```text
head01 only
```

---

# 157. Phase 2

Agent framework。

```text
node09
node02
```

---

# 158. Phase 3

Generic Entity + Capability model完成。

---

# 159. Phase 4

filesrv01/02。

```text
NFS server
storage dependency
```

---

# 160. Phase 5

全host agent展開。

---

# 161. Phase 6

Peer monitoring。

---

# 162. Phase 7

Diagnosis + Incident。

---

# 163. Phase 8

Dependency correlation。

---

# 164. Phase 9

Notifications。

---

# 165. Phase 10

Web UI。

---

# 166. MVP

MVP必須:

```text
single Rust binary

Environment model
ManagedEntity model
Host entity
Service entity

Capability model

generic dependency graph

Slurm discovery
agent registration
static inventory

network probe
Sentinel RPC
SSH probe
systemd probe
Slurm probe
NFS client/server probe
basic GPU probe

peer monitoring

observations
states
state transitions

diagnosis engine
incident engine
dependency correlation

SQLite
local spool
notifications
CLI
```

---

# 167. MVP Non-goals

```text
Ceph
Lustre
BeeGFS
IPMI
Redfish
InfiniBand
full ZFS telemetry
SMART fleet management
Prometheus replacement
Grafana replacement
automatic remediation
LLM diagnosis
HA Sentinel controller
```

ただしcore architectureで追加可能であること。

---

# 168. Acceptance: Current Cluster

現在の16 hostへ同一architecture用binaryを配布できる。

---

# 169. Acceptance: DRAIN

```text
Host healthy
Slurm DRAIN
```

を、

```text
SCHEDULER_DEGRADED
```

と判定。

---

# 170. Acceptance: slurmd

`slurmd`停止をhost failureと区別。

---

# 171. Acceptance: SSH

SSH停止をhost unreachableと区別。

---

# 172. Acceptance: NFS Server

filesrv01のNFS service停止を、

```text
NFS_SERVICE_FAILURE
```

と識別。

---

# 173. Acceptance: Shared Storage

filesrv01依存node群の同時異常をshared dependency incidentへ相関。

---

# 174. Acceptance: Client-local NFS

単一clientのみ異常ならfileserver障害と誤診断しない。

---

# 175. Acceptance: Path Failure

head01からのみhost unreachableならhost downと断定しない。

---

# 176. Acceptance: Controller Failure

Sentinel controller停止後もagent/peer observation継続。

---

# 177. Acceptance: Reboot

boot ID変更を正しく検出。

---

# 178. Acceptance: Topology Change

テスト環境で、

```text
filesrv01 dependency removal
new filesrv03 registration
```

を行ってもcore code変更なしで新topologyを反映可能。

---

# 179. Acceptance: Role Independence

`role=fileserver` を削除しても、

```text
storage.nfs.server
```

capabilityが存在すればNFS monitoringが機能すること。

---

# 180. Acceptance: Slurm Independence

Slurm外hostをSentinel inventoryへ正常に登録・監視できること。

---

# 181. Acceptance: Future Probe

Dummy capability/probeを追加した際、

```text
DB core
incident core
agent architecture
```

を変更せずObservationとして取り込めること。

---

# 182. Recommended Implementation Stack

```text
Rust
Tokio
axum
serde
clap
tracing
sqlx
SQLite
rustls
```

Frontend:

```text
React
TypeScript
```

---

# 183. Repository Structure

```text
/
├── Cargo.toml
├── src/
│   ├── main.rs
│   │
│   ├── environment/
│   ├── entity/
│   ├── capability/
│   ├── dependency/
│   ├── inventory/
│   │   ├── slurm/
│   │   ├── agent/
│   │   └── static_config/
│   │
│   ├── agent/
│   ├── controller/
│   ├── peer/
│   │
│   ├── probes/
│   │   ├── host/
│   │   ├── network/
│   │   ├── ssh/
│   │   ├── systemd/
│   │   ├── slurm/
│   │   ├── nfs/
│   │   ├── storage/
│   │   ├── gpu/
│   │   └── clock/
│   │
│   ├── observation/
│   ├── state/
│   ├── diagnosis/
│   ├── incident/
│   ├── correlation/
│   │
│   ├── notification/
│   ├── storage/
│   ├── api/
│   └── cli/
│
├── migrations/
├── tests/
├── fixtures/
├── docs/
└── packaging/
```

---

# 184. Core Dependency Direction

モジュール依存は原則、

```text
integrations/probes
        ↓
observation
        ↓
state
        ↓
diagnosis
        ↓
incident
```

とする。

CoreがSlurm/NFS implementationへ逆依存してはならない。

---

# 185. Coding Agentへの最重要指示

本プロジェクトを実装する際、

**現在のexample_clusterの具体的構成に最適化しすぎてcore architectureを固定化してはならない。**

現在のhost名、partition名、fileserver構成は、

```text
fixtures
deployment config
runtime discovery
tests
```

には使用してよい。

しかしcore logicへ埋め込んではならない。

---

# 186. 明示的禁止事項

以下を禁止する。

```text
・Hostを唯一のEntity typeとして固定する

・compute/fileserver/controllerを継承階層として作り込む

・roleによってprobeを直接分岐する

・IPをentity identityにする

・Slurm NodeNameとhostnameを同一と仮定する

・Slurm inventoryをSentinel inventory全体と同一視する

・NFSをStorage抽象の唯一実装として扱う

・filesrv01/filesrv02という名前をcore codeに書く

・-o/-i partition namingにcore logicを依存させる

・controller の host 名を source code へ埋め込む

・1 controllerしか存在できないDB schemaにする

・dependencyをtreeに限定する

・1 Host = 1 Serviceと仮定する

・全hostへ同じprobeを実行する

・probe failureでagent全体をpanicさせる

・外部commandをtimeoutなしで実行する

・NFS blocking syscallを無制限に生成する

・単一observer失敗でhost downを確定する

・network evidenceだけでpower offを確定する

・controller停止時にobservationsを失う

・v1でautomatic remediationを行う

・LLMをcore dependencyにする
```

---

# 187. 最終設計原則

Cluster Sentinelは、

```text
Slurm監視ツール
```

ではなく、

```text
汎用クラスタインフラ状態モデル
+
Slurm Integration
+
Storage Integration
+
Distributed Observation
+
Dependency-aware Incident Diagnosis
```

として構築する。

---

# 188. 現在への適合性と将来互換性

現在は、

```text
Slurm
+
NFS filesrv01
+
NFS filesrv02
+
GPU compute nodes
```

を主要対象とする。

将来、

```text
Slurm HA
+
CephFS
+
Lustre
+
login nodes
+
InfiniBand
+
BMC
+
storage cluster
+
複数scheduler
+
複数cluster
```

へ構成変更された場合も、

```text
Entity追加
Capability追加
Probe追加
Dependency変更
Configuration変更
```

で対応し、

```text
Observation
State
Diagnosis
Incident
Correlation
```

のcore architectureを原則変更しない。

---

# 189. 完成形

Cluster Sentinelは、

> **計算ノード、scheduler、controller、fileserver、storage service、将来的なnetwork/BMC等を汎用ManagedEntityとしてモデル化し、capabilityに応じたprobe、複数地点からの分散観測、依存関係グラフ、状態遷移、診断、インシデント相関を組み合わせることで、クラスタ構成の変化に強い長期運用可能なインフラ監視・障害診断基盤**

として実装する。

`example_cluster` は最初のproduction targetであるが、Sentinel coreの設計を `example_cluster` 固有のtopology、Slurm partition、host naming、NFS構成へ依存させてはならない。