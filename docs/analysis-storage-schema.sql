-- Design prototype only. Not a product migration or an implemented storage backend.
-- Runtime WAL/version validation, encoding, writer CAS, leases and GC are host duties.
PRAGMA foreign_keys = ON;

CREATE TABLE workspace_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    schema_version INTEGER NOT NULL,
    generation INTEGER NOT NULL CHECK(generation >= 0)
);
INSERT INTO workspace_state VALUES(1, 1, 0);

CREATE TABLE project (
    project_id INTEGER PRIMARY KEY,
    identity TEXT NOT NULL UNIQUE
);
CREATE TABLE source_file (
    file_id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project,
    relative_uri TEXT NOT NULL,
    UNIQUE(project_id, relative_uri)
);
CREATE TABLE build_variant (
    variant_id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project,
    variant_key BLOB NOT NULL,
    UNIQUE(project_id, variant_key)
);
CREATE TABLE analysis_unit (
    unit_id INTEGER PRIMARY KEY,
    variant_id INTEGER NOT NULL REFERENCES build_variant,
    owner_key TEXT NOT NULL,
    phase INTEGER NOT NULL CHECK(phase IN (1, 2, 3)),
    UNIQUE(variant_id, owner_key, phase)
);
CREATE TABLE analysis_version (
    version_id INTEGER PRIMARY KEY,
    unit_id INTEGER NOT NULL REFERENCES analysis_unit,
    input_key BLOB NOT NULL,
    analyzer_key BLOB NOT NULL,
    UNIQUE(unit_id, version_id)
);
CREATE TABLE unit_head (
    unit_id INTEGER PRIMARY KEY REFERENCES analysis_unit,
    version_id INTEGER NOT NULL UNIQUE,
    FOREIGN KEY(unit_id, version_id) REFERENCES analysis_version(unit_id, version_id)
);
CREATE TABLE unit_input (
    version_id INTEGER NOT NULL REFERENCES analysis_version ON DELETE CASCADE,
    file_id INTEGER NOT NULL REFERENCES source_file,
    content_hash BLOB NOT NULL,
    role TEXT NOT NULL,
    PRIMARY KEY(version_id, file_id)
) WITHOUT ROWID;
CREATE INDEX unit_input_by_file ON unit_input(file_id, version_id);

-- kind: 0=outline, 1=scope calls, 2=file metadata. scope=-1 means file scope.
-- Payload is bounded and versioned. Source text and ASTs are not stored here.
CREATE TABLE fact_chunk (
    version_id INTEGER NOT NULL REFERENCES analysis_version ON DELETE CASCADE,
    file_id INTEGER NOT NULL REFERENCES source_file,
    kind INTEGER NOT NULL,
    scope_local_id INTEGER NOT NULL,
    format_version INTEGER NOT NULL,
    encoding TEXT NOT NULL,
    payload BLOB NOT NULL,
    PRIMARY KEY(version_id, file_id, kind, scope_local_id)
) WITHOUT ROWID;

-- A2 semantic identity. A1 local IDs must not be treated as resolved targets.
CREATE TABLE symbol (
    symbol_id INTEGER PRIMARY KEY,
    variant_id INTEGER NOT NULL REFERENCES build_variant,
    logical_key BLOB NOT NULL,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    UNIQUE(variant_id, logical_key)
);
CREATE TABLE relation (
    edge_id INTEGER PRIMARY KEY,
    owner_version_id INTEGER NOT NULL REFERENCES analysis_version ON DELETE CASCADE,
    source_symbol_id INTEGER NOT NULL REFERENCES symbol,
    target_symbol_id INTEGER REFERENCES symbol,
    relation_kind INTEGER NOT NULL,
    resolution INTEGER NOT NULL,
    site_file_id INTEGER NOT NULL REFERENCES source_file,
    site_local_id INTEGER NOT NULL,
    start_byte INTEGER NOT NULL CHECK(start_byte >= 0),
    end_byte INTEGER NOT NULL CHECK(end_byte >= start_byte),
    evidence BLOB NOT NULL
);
-- Supports exact kind filtering and keyset pagination by edge_id in both directions.
CREATE INDEX relation_out ON relation(source_symbol_id, relation_kind, edge_id);
CREATE INDEX relation_in ON relation(target_symbol_id, relation_kind, edge_id);
-- Supports retiring a unit's old facts without scanning the global relation table.
CREATE INDEX relation_owner ON relation(owner_version_id);

-- Logical snapshots across requests. Publishing closes the prior interval and
-- inserts the new one in the SAME transaction that updates unit_head/generation.
CREATE TABLE unit_publication (
    unit_id INTEGER NOT NULL REFERENCES analysis_unit,
    begin_generation INTEGER NOT NULL CHECK(begin_generation >= 0),
    end_generation INTEGER CHECK(end_generation > begin_generation),
    version_id INTEGER NOT NULL,
    PRIMARY KEY(unit_id, begin_generation),
    FOREIGN KEY(unit_id, version_id) REFERENCES analysis_version(unit_id, version_id)
) WITHOUT ROWID;
CREATE INDEX publication_version ON unit_publication(version_id, begin_generation, end_generation);
