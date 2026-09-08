# Cluster Sentinel v0.3
## Implementation Supplement

`SPEC.md` が「何を作るか・どの設計原則に従うか」を規定するのに対し、本書は以下を規定する。

- 実装境界
- データ契約
- 通信契約
- 設定優先順位
- 永続化
- Probe実行方式
- 障害時挙動
- Docker Compose疑似クラスタ
- simulation / integration test
- 実装順序
- Definition of Done

本書と `SPEC.md` が競合する場合は、**SPEC.md のアーキテクチャ原則を優先する。**

---

# 1. Production Artifact

Production release artifactは原則、

```text
sentinel
```

の単一実行バイナリとする。

内部実装を複数module/crateへ分割することは許可するが、各監視対象hostに複数Sentinel実行ファイルを配布する設計は禁止する。

以下は同じbinaryのsubcommandとして提供する。

```bash
sentinel controller
sentinel agent

sentinel status
sentinel entity ...
sentinel incident ...
sentinel dependency ...
sentinel peers

sentinel doctor
sentinel config ...
sentinel version
```

---

# 2. Development Environment

Primary development environmentは、

```text
Windows
└── WSL2 Linux
    ├── Rust toolchain
    ├── Coding Agent
    ├── Cluster Sentinel repository
    └── Docker Engine / Docker Compose
```

を想定する。

実際のSlurm production nodeを日常的な開発環境として使用することを前提にしない。

---

# 3. Test Environment Hierarchy

テスト環境は以下の4層に分ける。

```text
Level 1
Unit / Mock Tests

Level 2
In-process Simulation

Level 3
Docker Compose Pseudo Cluster

Level 4
VM / Real Cluster Validation
```

各層の責務を明確に分離する。

---

# 4. Level 1 — Unit / Mock Tests

対象:

```text
domain model
configuration
Slurm parser
dependency graph
state engine
diagnosis rules
incident correlation
peer assignment
database
protocol serialization
```

外部サービスを必要としない。

CI上で必ず実行可能であること。

---

# 5. Level 2 — In-process Simulation

実際のnetwork/processを利用せず、Probe結果を人工的に生成するsimulation backendを実装する。

例:

```text
Host:
HEALTHY

SSH:
FAILED

Sentinel:
HEALTHY

Slurm:
IDLE
```

等のObservationを投入し、State / Diagnosis / Incidentが期待通り生成されるか検証する。

この層は高速かつdeterministicであること。

---

# 6. Level 3 — Docker Compose Pseudo Cluster

Docker Composeによる疑似クラスタを、**正式な開発成果物かつ必須integration testbed**として実装する。

目的:

- 複数host相当環境の再現
- Agent / Controller実通信
- Slurm実サービス
- SSH
- process/service failure
- peer monitoring
- network partition
- controller failure
- agent failure
- NFS論理障害
- incident correlation

をproduction clusterを壊さず検証すること。

---

# 7. Docker Compose Topology

最低限以下を提供する。

```text
sentinel-controller

compute01
compute02
compute03

filesrv01
filesrv02
```

概念構成:

```text
                      controller
                 Sentinel Controller
                      slurmctld
                          │
          ┌───────────────┼───────────────┐
          │               │               │
      compute01       compute02       compute03
      sentinel        sentinel        sentinel
       slurmd           slurmd          slurmd
          │               │               │
          └──────── peer monitoring ───────┘

             filesrv01        filesrv02
             sentinel         sentinel
```

必要に応じて規模を変更可能にする。

---

# 8. Docker Directory Layout

推奨:

```text
/
├── src/
├── docs/
├── tests/
│
└── dev/
    └── compose/
        ├── compose.yaml
        │
        ├── images/
        │   ├── base/
        │   ├── controller/
        │   ├── compute/
        │   └── fileserver/
        │
        ├── config/
        │   ├── slurm.conf
        │   ├── sentinel/
        │   └── ssh/
        │
        ├── scripts/
        │   ├── up
        │   ├── down
        │   ├── reset
        │   └── wait-healthy
        │
        └── scenarios/
            ├── stop-agent
            ├── stop-slurmd
            ├── drain-node
            ├── isolate
            ├── stop-fileserver
            ├── pause-host
            └── recover-all
```

---

# 9. Docker Testbed Requirements

以下を満たす。

```text
docker compose up
```

またはwrapper command一つで疑似クラスタを起動できること。

起動後、可能な範囲で自動health checkを行う。

Developerが各containerへ手作業で大量の初期設定を行う必要があってはならない。

---

# 10. Real Slurm in Docker

Docker疑似クラスタでは、Slurm parserだけでなく可能な範囲で本物の、

```text
munge
slurmctld
slurmd
```

を起動する。

目的は、

```text
slurmd停止
↓
Slurm node state変化
↓
Sentinel observation
↓
Diagnosis
```

というend-to-end挙動を検証すること。

Slurm CLIをmockするだけのテストとは別に扱う。

---

# 11. Container Roles

## Controller

少なくとも:

```text
Sentinel controller
Slurm CLI
slurmctld
munge
```

を持つ。

---

## Compute

少なくとも:

```text
Sentinel agent
slurmd
munge
SSH server
```

を持つ。

GPUは必須ではない。

---

## Fileserver

少なくとも:

```text
Sentinel agent
SSH server
NFS-related test service or simulated storage service
```

を持つ。

実NFS利用がWSL/Docker環境で不安定・危険な場合は、Docker integration layerではNFS service/network semanticsの再現を優先し、kernel-level NFS hang検証はVMへ委譲する。

---

# 12. Docker Is Not Production

Docker Composeはproduction deployment mechanismではない。

Productionでは、

```text
systemd
native Linux host
single sentinel binary
```

を基本とする。

Container-specific assumptionをSentinel coreへ持ち込んではならない。

---

# 13. Docker-specific Code Isolation

以下のようなDocker固有知識をcoreへ埋め込んではならない。

```text
container IDs
Docker network names
Compose service names
Docker socket
Docker API
```

Docker testbedはSentinelを外側から試験するfixture/infrastructureとして扱う。

---

# 14. Failure Injection Framework

Docker testbedには再利用可能なfailure injection mechanismを用意する。

最低限:

```text
agent failure
slurmd failure
Slurm DRAIN
host/container stop
host/container pause
controller failure
network isolation
one-direction or observer-specific network failure
fileserver service failure
```

をCLI/scriptから実行可能にする。

---

# 15. Scenario Commands

目標UX:

```bash
./dev/compose/scenarios/stop-agent compute01

./dev/compose/scenarios/stop-slurmd compute01

./dev/compose/scenarios/drain-node compute01

./dev/compose/scenarios/isolate sentinel-controller compute01

./dev/compose/scenarios/stop-fileserver filesrv01

./dev/compose/scenarios/pause-host compute01

./dev/compose/scenarios/recover-all
```

具体的なfile名は変更可能だが、同等の再現性を提供すること。

---

# 16. Declarative Scenario Tests

可能ならscenario expectationをmachine-readableにする。

例:

```yaml
name: slurmd_failure

target: compute01

expected:
  host: HEALTHY
  ssh: HEALTHY
  slurmd: FAILED

diagnoses:
  required:
    - SLURMD_SERVICE_FAILURE

  forbidden:
    - HOST_UNREACHABLE
```

---

# 17. Network Partition Scenarios

非常に重要なintegration testとする。

例:

```text
controller → compute01 FAIL

compute02 → compute01 OK

filesrv01 → compute01 OK
```

期待:

```text
PATH_SPECIFIC_NETWORK_FAILURE
```

またはcontroller-side connectivity degradation。

以下を誤って生成してはならない。

```text
HOST_UNREACHABLE
```

---

# 18. Full Host Failure Scenario

例:

```bash
docker stop compute01
```

または同等failure injection。

複数observerから到達不能になることを確認する。

期待:

```text
HOST_UNREACHABLE
```

ただし、

```text
POWER_OFF
```

とは判定しない。

---

# 19. Host Pause Scenario

```bash
docker pause compute01
```

等を利用して、process schedulingを含めた無応答状態を模擬する。

これは実kernel hard lockとは異なるが、

```text
network timeout
agent timeout
SSH timeout
peer failure
```

の複合状態テストとして使用する。

---

# 20. Agent Failure Scenario

Sentinel agentのみ停止。

期待:

```text
network      HEALTHY
SSH          HEALTHY
Slurm        HEALTHY
Sentinel     FAILED
```

Diagnosis:

```text
SENTINEL_AGENT_FAILURE
```

Host failureと誤診断しない。

---

# 21. SSH Failure Scenario

SSH serviceのみ停止。

期待:

```text
Sentinel RPC HEALTHY
Host         HEALTHY
SSH          FAILED
```

Diagnosis:

```text
SSH_SERVICE_FAILURE
```

---

# 22. slurmd Failure Scenario

`slurmd`のみ停止。

期待:

```text
Host       HEALTHY
Sentinel   HEALTHY
SSH        HEALTHY
slurmd     FAILED
```

Diagnosis:

```text
SLURMD_SERVICE_FAILURE
```

---

# 23. Slurm DRAIN Scenario

Slurm control planeからtest nodeをDRAINする。

期待:

```text
Host       HEALTHY
Sentinel   HEALTHY
SSH        HEALTHY
slurmd     HEALTHY
Slurm      DRAIN
```

Diagnosis:

```text
SLURM_ONLY_DEGRADATION
```

Classification:

```text
SCHEDULER_DEGRADED
```

---

# 24. Controller Failure Scenario

Sentinel controller processまたはcontroller containerを停止する。

Agent側で、

```text
self monitoring
peer monitoring
local spool
```

が継続すること。

Controller復旧後、Observationが再送されること。

---

# 25. Fileserver Failure Scenario

filesrv01のstorage/NFS serviceを停止する。

Host/container自体は生かしておく。

期待:

```text
filesrv01 host HEALTHY
Sentinel       HEALTHY
SSH            HEALTHY
storage/NFS    FAILED
```

Host failureとservice failureを区別する。

---

# 26. Shared Storage Failure Scenario

複数compute nodeが同じStorage Entityへ依存するtest topologyを作る。

例:

```text
compute01
compute02
    → storage01 → filesrv01
```

filesrv01/storage service障害時に、

```text
compute01 storage degradation
compute02 storage degradation
filesrv01 service failure
```

を、

```text
SHARED_STORAGE_FAILURE
```

として相関できること。

---

# 27. Client-only Storage Failure Scenario

compute01のみstorage accessを遮断する。

filesrv01とcompute02は正常。

期待:

```text
NFS_CLIENT_FAILURE
```

またはlocal client/storage-path diagnosis。

以下を生成してはならない。

```text
SHARED_STORAGE_FAILURE
```

---

# 28. Dockerで保証しないもの

以下についてDockerだけで実機同等性を保証してはならない。

```text
real machine reboot semantics
boot ID change
kernel hard lock
true D-state behavior
NFS hard-mount kernel stall
real systemd host behavior
SMART/NVMe failure
physical GPU failure
IPMI/BMC
NIC hardware failure
physical power loss
```

これらはVMまたは実cluster validationへ委譲する。

---

# 29. Level 4 — VM / Real Cluster Validation

Dockerでは十分に再現できない機能を検証する。

主対象:

```text
systemd behavior
real reboot
boot ID
NFS hard mount
D-state
kernel/journal behavior
real GPU
NVMe/SMART
actual Slurm daemon interactions
```

---

# 30. VM Testbed

必要になった段階で小規模VM環境を使用する。

例:

```text
ctrl01
compute01
filesrv01
```

程度でもよい。

VM testbedを日常的な主開発環境にすることは必須ではない。

---

# 31. Real Cluster Validation

`mizuno_cluster` は開発環境ではなく、

```text
staging / production validation environment
```

として扱う。

導入順:

```text
read-only controller
↓
single agent
↓
multiple agents
↓
peer monitoring
↓
safe fault injection
↓
full deployment
```

---

# 32. Production Fault Injection

実clusterで危険なfailure simulationを自動実行してはならない。

特に、

```text
NFS server shutdown
network-wide iptables changes
reboot
filesystem manipulation
```

等は管理者判断下のみ。

Docker/VM testで代替できるものはそちらで行う。

---

# 33. Initial Implementation Strategy

初期実装では過度な抽象化を避ける。

MVPでは以下を実装しない。

```text
dynamic shared-library plugin system
WASM plugin system
generic rule DSL
distributed consensus
controller HA
arbitrary remote command execution
```

ProbeやDiagnosisRuleはbinaryへstatic compileする。

---

# 34. Recommended Rust Stack

基本候補:

```text
tokio
axum
serde
serde_json
toml
clap
tracing
tracing-subscriber
sqlx
rustls
uuid
thiserror
```

原則:

- core/library層ではtyped error
- CLI/application boundaryでは `anyhow` 等を利用してよい
- `unwrap()` / `expect()` は限定使用
- probe failureでdaemon全体をpanicさせない

---

# 35. Repository Layout

推奨:

```text
/
├── Cargo.toml
├── README.md
├── CHANGELOG.md
│
├── docs/
│   ├── SPEC.md
│   ├── IMPLEMENTATION.md
│   ├── ARCHITECTURE.md
│   ├── CONFIGURATION.md
│   ├── OPERATIONS.md
│   ├── SECURITY.md
│   └── DEVELOPMENT.md
│
├── src/
│   ├── main.rs
│   ├── config/
│   ├── entity/
│   ├── capability/
│   ├── dependency/
│   ├── inventory/
│   ├── agent/
│   ├── controller/
│   ├── protocol/
│   ├── probes/
│   ├── observation/
│   ├── state/
│   ├── diagnosis/
│   ├── incident/
│   ├── correlation/
│   ├── notification/
│   ├── persistence/
│   ├── api/
│   └── cli/
│
├── migrations/
│
├── fixtures/
│   ├── slurm/
│   └── mizuno_cluster/
│
├── tests/
│   ├── integration/
│   ├── simulation/
│   └── scenarios/
│
└── dev/
    └── compose/
```

---

# 36. Core Domain Types

以下を明示的domain typeとして実装する。

```text
Environment
Cluster
ManagedEntity
Capability
DependencyEdge

ProbeDefinition
Observation

EntityState
StateTransition

Diagnosis
Incident

AgentIdentity
AgentSession
PeerAssignment
```

単なる `HashMap<String, Value>` の集合だけでdomain modelを構成してはならない。

---

# 37. ManagedEntity

最低限:

```text
id
environment_id
cluster_id optional
entity_type
canonical_name
display_name
labels
metadata
lifecycle_state
created_at
updated_at
```

Entity type:

```text
host
service
storage
scheduler
external_dependency
```

---

# 38. Entity Identity

DB内部ではUUID等を使用する。

Discovery merge用natural key:

```text
environment
+
entity_type
+
canonical_name
```

IP addressをHost identityとして使用しない。

Slurm NodeNameとhostnameも分離する。

---

# 39. Capability

Namespaced stringとする。

例:

```text
host.metrics
network.tcp
ssh.server
systemd

slurm.controller
slurm.compute

storage.local
storage.nfs.client
storage.nfs.server
storage.zfs
storage.smart

gpu.nvidia

journal.read
observer.peer
notification.fallback
```

---

# 40. Capability Resolution

優先順位:

```text
explicit force-disable
>
explicit force-enable
>
runtime discovery
>
role-derived hint
```

Roleのみを根拠にprobeを起動しない。

---

# 41. DependencyEdge

最低限:

```text
id
source_entity_id
target_entity_id
dependency_type
criticality
metadata
discovery_source
first_seen_at
last_seen_at
```

方向:

```text
A depends on B

A → B
```

Graphはcycleを許容する。

Traversalではcycle protection必須。

---

# 42. Probe Interface

概念:

```rust
trait Probe {
    fn id(&self) -> ProbeId;
    fn required_capabilities(&self) -> ...;
    fn default_interval(&self) -> Duration;
    fn default_timeout(&self) -> Duration;

    async fn collect(&self, context: &ProbeContext) -> ProbeResult;
}
```

実際のRust interfaceはobject safety等に合わせて調整可能。

---

# 43. Probe Result

最低限:

```text
observation_id
probe_id
target_entity_id
observer_entity_id optional
agent_session_id

started_at
finished_at
duration

status
structured_payload
evidence
error_code optional
error_message optional
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

# 44. Observation Immutability

保存済みObservationを書き換えない。

後から変更可能なのは、

```text
derived state
diagnosis
incident correlation
```

のみ。

---

# 45. Idempotent Ingestion

Observationにはglobal unique IDを持たせる。

Agent spool再送でも二重挿入しない。

Controller ingestionはidempotentとする。

---

# 46. Time Model

保存timestampはUTC。

Probe latency/timeoutはmonotonic clockを使用。

Incident timelineはwall clock。

---

# 47. Clock Skew

Agent/controller間のwall-clock差を監視する。

閾値超過:

```text
CLOCK_SKEW
```

Clock skewがあってもObservationを破棄しない。

---

# 48. Probe Scheduler

Probeごとに、

```text
interval
timeout
jitter
enabled
```

を管理する。

global timer loopへの直書きは禁止。

---

# 49. Probe Isolation

1つのprobeが、

```text
panic
timeout
external command hang
filesystem stall
```

しても他probeを停止させない。

---

# 50. Common Command Runner

External command実行を共通化する。

必須:

```text
timeout
stdout size limit
stderr size limit
exit status
execution duration
allowlisted command
structured error
```

Probeごとに無秩序な `Command::new()` を実装しない。

---

# 51. Output Limits

以下の巨大出力対策を行う。

```text
journalctl
squeue
sacct
zpool
```

Truncate時はその事実をObservationへ記録する。

---

# 52. Journal Safety

Incident evidence取得時:

```text
time range
unit
priority
line/byte limit
```

で制限する。

候補:

```text
±5 minutes
500 lines
1 MiB
```

configurable。

---

# 53. NFS Probe Safety

NFS mountごとに、

```text
max outstanding active filesystem probe = 1
```

とする。

前probeがSTUCKなら新規filesystem syscall probeを作らない。

TCP/2049等のnon-filesystem probeは継続可能。

---

# 54. Agent Local Spool

Controller unreachable時のObservationを、

```text
SQLite spool
```

へ保存する。

例:

```text
/var/lib/sentinel/spool.db
```

要件:

```text
WAL
bounded retention
idempotent resend
ordered replay
crash recovery
```

---

# 55. Spool Capacity

無限成長禁止。

```text
maximum age
maximum rows
maximum bytes
```

をconfigurableにする。

重大Event/Transitionは通常metricsより保持優先度を高くする。

---

# 56. Controller Database

MVP:

```text
SQLite + WAL
```

例:

```text
/var/lib/sentinel/sentinel.db
```

Schema変更はmigrationのみ。

---

# 57. Configuration Versioning

Config:

```toml
config_version = 1
```

DB migration versionも保持する。

Future config versionを黙って読み込まない。

---

# 58. Configuration Precedence

```text
CLI
>
environment variable
>
config file
>
runtime discovery
>
built-in default
```

Debug時にvalue sourceを確認可能にする。

---

# 59. Default Paths

```text
/etc/sentinel/config.toml

/var/lib/sentinel/

/run/sentinel/
```

Loggingは基本journald。

---

# 60. Config Validation

```bash
sentinel config check
```

で、

```text
syntax
invalid durations
invalid endpoint
duplicate entity
bad dependency
security configuration
```

等を検証。

---

# 61. Controller-Agent Protocol

MVP:

```text
versioned HTTPS API
+
JSON
```

protobuf/gRPCは将来追加可能。

---

# 62. Protocol Version

例:

```text
/v1/...
```

Binary versionとprotocol versionを分離する。

---

# 63. Minimum API

概念:

```text
POST /v1/agents/register
POST /v1/agents/heartbeat
POST /v1/observations/batch

GET /v1/agents/{id}/assignments

GET /v1/health
GET /v1/peer/health
```

---

# 64. Agent Registration

報告:

```text
agent version
protocol version
environment
hostname
boot ID
addresses
capabilities
hardware summary
```

Controllerがinventoryへmergeする。

---

# 65. Authentication

完全unauthenticatedは禁止。

MVPでは、

```text
TLS
+
cluster-scoped credential
```

でもよい。

将来mTLS/per-node credentialへ交換可能な設計とする。

Secretをbinaryへ埋め込まない。

---

# 66. Peer Assignment

deterministicかつtopology-awareなassignmentを目標とする。

各targetにつき可能なら、

```text
same dependency domain
different dependency domain
independent observer
```

から観測者を選ぶ。

Default degree:

```text
3
```

---

# 67. Peer Assignment Safety

避ける:

```text
self peer
duplicate observer
all observers in same failure domain
```

Assignment revisionを持つ。

---

# 68. State Engine

Probeはraw factのみ返す。

Probe内で、

```text
HOST_UNREACHABLE
SHARED_STORAGE_FAILURE
```

等を直接生成しない。

---

# 69. State Components

最低限:

```text
availability
network
host
agent
ssh
service
scheduler
storage
accelerator
clock
```

NOT_APPLICABLE対応必須。

---

# 70. Debounce / Hysteresis

初期default:

```text
warning = 2 failures
critical = 3 failures
recovery = 2 successes
```

Slurm DRAIN等の明示状態は即時Event化可能。

---

# 71. Diagnosis Rules

MVPではtyped Rust rules。

Generic DSLは作らない。

最低限:

```text
HOST_UNREACHABLE
PATH_SPECIFIC_NETWORK_FAILURE

SSH_SERVICE_FAILURE
SENTINEL_AGENT_FAILURE

SLURMD_SERVICE_FAILURE
SLURM_ONLY_DEGRADATION
SLURM_CONTROL_PLANE_FAILURE

RESOURCE_CONFIGURATION_MISMATCH

NFS_SERVICE_FAILURE
NFS_CLIENT_FAILURE
SHARED_STORAGE_FAILURE

GPU_CONFIGURATION_MISMATCH

CLOCK_SKEW
```

---

# 72. Diagnosis Evidence

Diagnosisは、

```text
evidence observation IDs
affected entities
suspected root entities
rule ID
confidence
```

を保持する。

自然言語だけを保存しない。

---

# 73. Confidence

```text
LOW
MEDIUM
HIGH
CONFIRMED
```

`CONFIRMED` は直接evidenceがある場合のみ。

Peer reachabilityだけでは通常 `HIGH` まで。

---

# 74. Incident Correlation

利用:

```text
time proximity
shared dependency
same failed service
same scheduler
same network failure pattern
```

---

# 75. Incident Lifecycle

```text
OPEN
ACKNOWLEDGED
RECOVERING
RESOLVED
SUPPRESSED
```

Root service復旧だけで依存clients未復旧ならRESOLVEDにしない。

---

# 76. Maintenance

Maintenance中もObservationを収集する。

抑制するのは基本notification。

異常stateをHEALTHYへ書き換えない。

---

# 77. Notifications

Provider abstractionを使用。

MVP推奨:

```text
generic webhook
```

または、

```text
ntfy
```

通知対象:

```text
new incident
severity escalation
meaningful diagnosis change
recovery
resolution
```

Pollingごとの再通知は禁止。

---

# 78. Slurm Parsing

Unknown fieldやfield ordering changeへ寛容にする。

実Slurm output fixtureを保存する。

最低限:

```text
IDLE
ALLOCATED
MIXED
DRAIN
DOWN
NOT_RESPONDING
INVALID_REG
```

---

# 79. Current Mizuno Fixture

現在の実構成は、

```text
fixtures/mizuno_cluster/
```

へtest fixtureとして保存してよい。

ただしcoreから参照しない。

---

# 80. GPU Probe

MVPは `nvidia-smi` でよい。

Expected GPUなしなら、

```text
NOT_APPLICABLE
```

Expected GPUありなのにcommand/deviceが無ければ異常候補。

---

# 81. File Server Observations

巨大な単一 `fileserver_health` probeは禁止。

以下を分ける。

```text
host
service
TCP
export
backing filesystem
capacity
latency
```

---

# 82. Logging

`tracing` 使用。

Context:

```text
entity_id
agent_id
probe_id
incident_id
request_id
```

Secretや巨大command outputはlogしない。

---

# 83. Self Monitoring

Sentinel自身について、

```text
uptime
version
spool size
queue depth
DB state
last controller connection
probe failures
```

を確認可能にする。

---

# 84. Graceful Shutdown

SIGTERM時:

```text
stop new work
cancel/finish probes
flush important data
close DB safely
```

---

# 85. systemd

Production deploymentはsystemd前提。

可能な限りhardening:

```text
NoNewPrivileges
PrivateTmp
ProtectHome
ProtectSystem
ProtectKernelTunables
ProtectControlGroups
RestrictSUIDSGID
```

必要pathのみwrite許可。

---

# 86. CI

最低限:

```bash
cargo fmt --check

cargo clippy \
  --all-targets \
  --all-features \
  -- \
  -D warnings

cargo test --all
```

---

# 87. Docker Tests in CI

可能なCI環境では、

```text
docker compose pseudo-cluster smoke test
```

も実行する。

少なくとも、

```text
cluster startup
agent registration
Slurm discovery
basic peer communication
one or more failure scenarios
```

をCI integration jobとして実行可能にする。

Docker利用不可のCIでもunit/mock testsは通るよう分離する。

---

# 88. Test Pyramid

必須:

```text
unit
mock
simulation
parser fixtures
DB migration
protocol
Docker Compose integration
scenario tests
```

追加:

```text
VM
real cluster staged test
```

---

# 89. Scenario Golden Tests

代表状態を固定する。

例:

```text
Host OK
SSH OK
Agent OK
slurmd OK
Slurm DRAIN
```

期待:

```text
SCHEDULER_DEGRADED
SLURM_ONLY_DEGRADATION
```

---

# 90. Resource Budget

Agent通常時目標:

```text
RAM < 100 MiB desirable
idle CPU substantially below one core
minimal disk writes
minimal network traffic
```

---

# 91. Web UI

Core完成後。

順序:

```text
CLI
→ API
→ Web UI
```

Docker testbedの存在を理由にUIを先行させない。

---

# 92. Documentation

v1までに最低限:

```text
README.md
docs/ARCHITECTURE.md
docs/CONFIGURATION.md
docs/OPERATIONS.md
docs/SECURITY.md
docs/DEVELOPMENT.md
```

`DEVELOPMENT.md` にはDocker Compose testbedの利用方法を必ず含める。

---

# 93. Development Documentation Requirements

`docs/DEVELOPMENT.md` に最低限記載:

```text
WSL/Linux prerequisites

Rust toolchain

Docker prerequisites

pseudo-cluster startup

pseudo-cluster shutdown

scenario execution

reset procedure

running unit tests

running Docker integration tests

known Docker limitations

when VM testing is required
```

---

# 94. Implementation Milestones

## M0 — Repository / Core Domain

```text
CLI skeleton
configuration
domain models
SQLite migrations
logging
```

Done:

```bash
sentinel version
sentinel config check
```

---

## M1 — Passive Controller + Slurm

```text
controller
Slurm discovery
Slurm parser
inventory
persistence
CLI status
```

Done:

```bash
sentinel status
```

でfixtureまたは実Slurmからstate表示。

---

## M2 — Agent + Protocol

```text
agent daemon
registration
heartbeat
local spool
runtime discovery
```

---

## M3 — Docker Compose Pseudo Cluster

このmilestoneを正式に追加する。

実装:

```text
Compose topology
controller container
compute containers
fileserver containers
real Slurm services where practical
SSH
Sentinel agent/controller
health checks
reset scripts
```

Done:

```bash
docker compose up
```

相当で疑似クラスタが再現可能。

Controllerへ複数Agentが登録され、

```bash
sentinel status
```

で認識できる。

---

## M4 — Basic Host Monitoring

```text
Sentinel RPC
network
SSH
systemd/process
host metrics
```

Docker上でagent/SSH failure scenarioを通す。

---

## M5 — Slurm Diagnosis

```text
slurmd
DRAIN
controller state
resource mismatch
```

Docker上の実Slurm testbedで、

```text
stop-slurmd
drain-node
```

scenarioを通す。

---

## M6 — Storage / NFS

```text
Storage entities
DependencyGraph
NFS client
NFS server
safe active probing
```

Dockerではservice/network-level NFS scenarioを実装。

Kernel-level hard mount挙動はVM test requirementとして記録。

---

## M7 — GPU

```text
NVIDIA discovery
GPU probe
Slurm GRES comparison
```

Dockerではmock中心。

実GPUはreal cluster validation。

---

## M8 — Peer Monitoring

```text
assignment
remote observation
quorum
path failure detection
```

Docker network isolation scenarioを必須とする。

---

## M9 — Diagnosis / Incident Correlation

```text
state engine
diagnosis rules
shared dependency correlation
incident lifecycle
evidence preservation
```

Docker scenariosからend-to-end incident生成まで検証。

---

## M10 — Notification / Operations

```text
notification
deduplication
maintenance
installer
systemd
operations docs
```

---

## M11 — VM / Production Validation

Dockerでは検証できない、

```text
systemd
boot ID
reboot
NFS hard mount
kernel events
real GPU
```

を段階的に検証する。

---

## M12 — Web UI

Core acceptance tests通過後のみ。

---

# 95. Definition of Done Per Milestone

各milestoneは、

```text
code
tests
documentation
migration if necessary
```

が揃うまでDoneではない。

Docker関連milestoneでは、

```text
reproducible Compose environment
automated scenario
expected result
cleanup/reset procedure
```

まで含める。

---

# 96. Global v1 Acceptance

最低限以下を確認する。

### Slurm DRAIN

```text
Host healthy
SSH healthy
Agent healthy
slurmd healthy
Slurm DRAIN
```

を正しく分類。

### slurmd Failure

Host failureと区別。

### SSH Failure

Agent reachableだがSSHだけ異常を識別。

### Agent Failure

SSH/Slurm正常でAgentだけ異常。

### Host Unreachable

複数observerから到達不能。

### Path Failure

一部observerのみ到達不能。

### NFS Server Failure

fileserver hostとNFS serviceを区別。

### Shared Storage Failure

複数client障害をdependency-aware incidentへ相関。

### Client-local NFS Failure

fileserver障害と誤診断しない。

### Controller Failure

local spoolとpeer monitoring継続。

### Reboot

VMまたは実機でboot ID変更を検出。

### Topology Change

host/storage追加にcore変更不要。

---

# 97. Docker-specific v1 Acceptance

以下を一連のautomationとして実行可能であること。

```text
1. Pseudo cluster startup

2. All agents registered

3. Baseline HEALTHY

4. Stop Sentinel agent on compute01

5. SENTINEL_AGENT_FAILURE detected

6. Recover compute01

7. Stop slurmd

8. SLURMD_SERVICE_FAILURE detected

9. Recover slurmd

10. Slurm DRAIN

11. SLURM_ONLY_DEGRADATION detected

12. Isolate controller→compute01 only

13. PATH_SPECIFIC_NETWORK_FAILURE detected

14. Stop filesrv01 storage service

15. Shared-storage incident detected where topology applies

16. Recover all

17. Incidents resolve

18. Environment cleanup succeeds
```

---

# 98. Implementation Priority

迷った場合:

```text
correctness
>
failure isolation
>
diagnostic usefulness
>
security
>
operability
>
testability
>
performance
>
UI polish
```

---

# 99. Avoid Premature Complexity

MVPでは作らない:

```text
plugin marketplace
custom rule language
custom TSDB
distributed DB
full Prometheus replacement
full Grafana replacement
automatic repair
LLM subsystem
Sentinel HA
```

---

# 100. Preserve Extensibility

拡張点:

```text
InventoryProvider
Probe
DiagnosisRule
NotificationProvider
PersistenceRepository
AuthenticationProvider
```

ただしinterfaceだけの巨大frameworkを先に作らない。

---

# 101. Deployment Data Must Remain Data

以下はcore source codeへ埋め込まない。

```text
parent
filesrv01
filesrv02

creator2-7
andre01
david01
david02
grace01
grace02
preproc01
hiegm5

current Slurm partitions
current NFS topology
current IP addresses
```

Fixture/config/runtime discoveryで扱う。

---

# 102. Final Implementation Principle

全実装を通して、

```text
Probe
↓
Observation
↓
State
↓
Diagnosis
↓
Incident
```

の責務分離を維持する。

また、

```text
Slurm / NFS / NVIDIA / Docker test infrastructure
```

と、

```text
Sentinel core
```

を分離する。

Docker ComposeはSentinel architectureそのものではなく、

> **Sentinelを安全かつ再現可能に壊して試すための第一級development/test infrastructure**

として実装・維持する。

日常的な開発・Coding Agentによる自動検証はWSL + Docker Composeを中心に行い、Dockerで再現できないkernel/systemd/hardwareレベルの挙動のみVMおよび実clusterで段階的に検証する。