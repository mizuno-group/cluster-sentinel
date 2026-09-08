-- Cluster Sentinel core schema.
--
-- Design notes that are load-bearing:
--
--  * Entity identity is (environment, entity_type, canonical_name). An IP
--    address is a row in entity_addresses, never a key (SPEC.md §37).
--  * dependencies is a general directed graph: cycles are permitted and
--    traversal code is responsible for terminating (SPEC.md §28).
--  * controllers has no uniqueness constraint forcing a single controller per
--    environment; a future HA or multi-collector deployment must not require a
--    migration of this table (SPEC.md §43, §186).
--  * observations are append-only. Nothing in this schema updates them.

CREATE TABLE environments (
    name        TEXT PRIMARY KEY,
    display_name TEXT,
    metadata    TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL
) STRICT;

CREATE TABLE clusters (
    id          TEXT PRIMARY KEY,
    environment TEXT NOT NULL REFERENCES environments(name) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    metadata    TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL,
    UNIQUE (environment, name)
) STRICT;

-- Several controllers may serve one environment. Deliberately not UNIQUE on
-- environment.
CREATE TABLE controllers (
    id            TEXT PRIMARY KEY,
    environment   TEXT NOT NULL REFERENCES environments(name) ON DELETE CASCADE,
    endpoint      TEXT NOT NULL,
    role          TEXT NOT NULL DEFAULT 'primary',
    last_seen_at  TEXT,
    created_at    TEXT NOT NULL
) STRICT;

CREATE TABLE entities (
    id              TEXT PRIMARY KEY,
    environment     TEXT NOT NULL REFERENCES environments(name) ON DELETE CASCADE,
    cluster         TEXT,
    entity_type     TEXT NOT NULL,
    canonical_name  TEXT NOT NULL,
    display_name    TEXT NOT NULL,
    metadata        TEXT NOT NULL DEFAULT '{}',
    lifecycle_state TEXT NOT NULL DEFAULT 'active',
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE (environment, entity_type, canonical_name)
) STRICT;

CREATE INDEX idx_entities_type ON entities(environment, entity_type);
CREATE INDEX idx_entities_lifecycle ON entities(lifecycle_state);

CREATE TABLE entity_labels (
    entity_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL,
    PRIMARY KEY (entity_id, key)
) STRICT;

CREATE TABLE entity_capabilities (
    entity_id        TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    capability       TEXT NOT NULL,
    resolution_reason TEXT NOT NULL,
    discovery_source TEXT NOT NULL,
    first_seen_at    TEXT NOT NULL,
    last_seen_at     TEXT NOT NULL,
    PRIMARY KEY (entity_id, capability)
) STRICT;

CREATE INDEX idx_entity_capabilities_capability ON entity_capabilities(capability);

-- One entity legitimately has many addresses across management, storage and
-- interconnect networks.
CREATE TABLE entity_addresses (
    entity_id  TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    address    TEXT NOT NULL,
    family     TEXT NOT NULL DEFAULT 'unknown',
    network    TEXT,
    is_primary INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (entity_id, address)
) STRICT;

CREATE TABLE entity_discovery_sources (
    entity_id  TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    source     TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at  TEXT NOT NULL,
    PRIMARY KEY (entity_id, source)
) STRICT;

CREATE TABLE dependencies (
    id               TEXT PRIMARY KEY,
    source_entity_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    target_entity_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    dependency_type  TEXT NOT NULL,
    criticality      TEXT NOT NULL DEFAULT 'critical',
    metadata         TEXT NOT NULL DEFAULT '{}',
    discovery_source TEXT NOT NULL,
    first_seen_at    TEXT NOT NULL,
    last_seen_at     TEXT NOT NULL,
    UNIQUE (source_entity_id, target_entity_id, dependency_type)
) STRICT;

CREATE INDEX idx_dependencies_source ON dependencies(source_entity_id);
CREATE INDEX idx_dependencies_target ON dependencies(target_entity_id);

CREATE TABLE agent_instances (
    id             TEXT PRIMARY KEY,
    entity_id      TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    agent_version  TEXT NOT NULL,
    protocol_version INTEGER NOT NULL,
    first_seen_at  TEXT NOT NULL,
    last_seen_at   TEXT NOT NULL
) STRICT;

-- A new boot id means the host rebooted; sessions make that observable
-- without guessing (SPEC.md §107).
CREATE TABLE agent_sessions (
    id            TEXT PRIMARY KEY,
    agent_id      TEXT NOT NULL REFERENCES agent_instances(id) ON DELETE CASCADE,
    boot_id       TEXT,
    started_at    TEXT NOT NULL,
    last_heartbeat_at TEXT,
    ended_at      TEXT
) STRICT;

CREATE INDEX idx_agent_sessions_agent ON agent_sessions(agent_id);

CREATE TABLE probes (
    id          TEXT PRIMARY KEY,
    definition  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
) STRICT;

-- Append-only. observation ids are supplied by the producing agent so that
-- spool replay is idempotent (IMPLEMENTATION.md §45).
CREATE TABLE observations (
    id                 TEXT PRIMARY KEY,
    probe_id           TEXT NOT NULL,
    target_entity_id   TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    observer_entity_id TEXT REFERENCES entities(id) ON DELETE SET NULL,
    agent_session_id   TEXT,
    started_at         TEXT NOT NULL,
    finished_at        TEXT NOT NULL,
    duration_ms        INTEGER NOT NULL,
    status             TEXT NOT NULL,
    payload            TEXT NOT NULL DEFAULT '{}',
    evidence           TEXT NOT NULL DEFAULT '{}',
    error_code         TEXT,
    error_message      TEXT,
    ingested_at        TEXT NOT NULL
) STRICT;

CREATE INDEX idx_observations_target_time ON observations(target_entity_id, finished_at DESC);
CREATE INDEX idx_observations_probe_time ON observations(probe_id, finished_at DESC);
CREATE INDEX idx_observations_observer ON observations(observer_entity_id, finished_at DESC);

CREATE TABLE entity_states (
    entity_id    TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    component    TEXT NOT NULL,
    health       TEXT NOT NULL,
    since        TEXT NOT NULL,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    consecutive_successes INTEGER NOT NULL DEFAULT 0,
    evidence     TEXT NOT NULL DEFAULT '[]',
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (entity_id, component)
) STRICT;

CREATE TABLE entity_overall_states (
    entity_id       TEXT PRIMARY KEY REFERENCES entities(id) ON DELETE CASCADE,
    health          TEXT NOT NULL,
    classifications TEXT NOT NULL DEFAULT '[]',
    since           TEXT NOT NULL,
    updated_at      TEXT NOT NULL
) STRICT;

CREATE TABLE state_transitions (
    id         TEXT PRIMARY KEY,
    entity_id  TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    component  TEXT,
    from_health TEXT NOT NULL,
    to_health   TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    evidence    TEXT NOT NULL DEFAULT '[]'
) STRICT;

CREATE INDEX idx_state_transitions_entity_time ON state_transitions(entity_id, occurred_at DESC);

CREATE TABLE diagnoses (
    id             TEXT PRIMARY KEY,
    diagnosis_type TEXT NOT NULL,
    rule_id        TEXT NOT NULL,
    confidence     TEXT NOT NULL,
    summary        TEXT NOT NULL DEFAULT '',
    recommended_actions TEXT NOT NULL DEFAULT '[]',
    incident_id    TEXT,
    created_at     TEXT NOT NULL
) STRICT;

CREATE INDEX idx_diagnoses_type_time ON diagnoses(diagnosis_type, created_at DESC);
CREATE INDEX idx_diagnoses_incident ON diagnoses(incident_id);

CREATE TABLE diagnosis_entities (
    diagnosis_id TEXT NOT NULL REFERENCES diagnoses(id) ON DELETE CASCADE,
    entity_id    TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    role         TEXT NOT NULL,
    PRIMARY KEY (diagnosis_id, entity_id, role)
) STRICT;

CREATE TABLE diagnosis_evidence (
    diagnosis_id   TEXT NOT NULL REFERENCES diagnoses(id) ON DELETE CASCADE,
    observation_id TEXT NOT NULL,
    PRIMARY KEY (diagnosis_id, observation_id)
) STRICT;

CREATE TABLE incidents (
    id           TEXT PRIMARY KEY,
    environment  TEXT NOT NULL REFERENCES environments(name) ON DELETE CASCADE,
    fingerprint  TEXT NOT NULL,
    status       TEXT NOT NULL,
    severity     TEXT NOT NULL,
    started_at   TEXT NOT NULL,
    ended_at     TEXT,
    updated_at   TEXT NOT NULL
) STRICT;

CREATE INDEX idx_incidents_status ON incidents(status, started_at DESC);
CREATE INDEX idx_incidents_fingerprint ON incidents(environment, fingerprint, status);

CREATE TABLE incident_entities (
    incident_id TEXT NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    entity_id   TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    role        TEXT NOT NULL,
    PRIMARY KEY (incident_id, entity_id, role)
) STRICT;

CREATE TABLE incident_evidence (
    incident_id    TEXT NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    observation_id TEXT NOT NULL,
    PRIMARY KEY (incident_id, observation_id)
) STRICT;

CREATE TABLE incident_timeline (
    id          TEXT PRIMARY KEY,
    incident_id TEXT NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    occurred_at TEXT NOT NULL,
    kind        TEXT NOT NULL,
    detail      TEXT NOT NULL DEFAULT '',
    entity_id   TEXT
) STRICT;

CREATE INDEX idx_incident_timeline_incident ON incident_timeline(incident_id, occurred_at);

CREATE TABLE notifications (
    id               TEXT PRIMARY KEY,
    incident_id      TEXT NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    provider         TEXT NOT NULL,
    deduplication_key TEXT NOT NULL,
    status           TEXT NOT NULL,
    sent_at          TEXT,
    error            TEXT,
    created_at       TEXT NOT NULL,
    UNIQUE (provider, deduplication_key)
) STRICT;

CREATE TABLE acknowledgements (
    id          TEXT PRIMARY KEY,
    incident_id TEXT NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
    actor       TEXT NOT NULL,
    note        TEXT,
    created_at  TEXT NOT NULL
) STRICT;

-- Maintenance suppresses notification, never observation or state
-- (IMPLEMENTATION.md §76).
CREATE TABLE maintenance_windows (
    id          TEXT PRIMARY KEY,
    environment TEXT NOT NULL REFERENCES environments(name) ON DELETE CASCADE,
    entity_id   TEXT REFERENCES entities(id) ON DELETE CASCADE,
    reason      TEXT NOT NULL DEFAULT '',
    starts_at   TEXT NOT NULL,
    ends_at     TEXT,
    created_by  TEXT,
    created_at  TEXT NOT NULL
) STRICT;

CREATE INDEX idx_maintenance_active ON maintenance_windows(environment, starts_at, ends_at);

CREATE TABLE peer_assignments (
    id           TEXT PRIMARY KEY,
    revision     INTEGER NOT NULL,
    target_entity_id   TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    observer_entity_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    reason       TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL,
    UNIQUE (revision, target_entity_id, observer_entity_id)
) STRICT;

CREATE INDEX idx_peer_assignments_observer ON peer_assignments(observer_entity_id, revision);
