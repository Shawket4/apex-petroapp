-- Tables fully owned by the apex-petroapp service. None are touched by Falcon's
-- GORM layer; this service is the single writer.

-- Credentials: manually-captured PetroApp bearer token + cookie (single row).
-- Replaces Falcon's `petro_app_tokens`.
CREATE TABLE IF NOT EXISTS petroapp_credentials (
    id                SMALLINT     PRIMARY KEY DEFAULT 1,
    token             TEXT         NOT NULL,
    cookie            TEXT         NOT NULL DEFAULT '',
    status            TEXT         NOT NULL DEFAULT 'unknown', -- unknown | valid | expired
    last_validated_at TIMESTAMPTZ,
    updated_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    CONSTRAINT petroapp_credentials_singleton CHECK (id = 1)
);

-- Vehicle map: replaces the hardcoded Go `PlateNumberMap`. Links the PetroApp
-- numeric vehicle id to a row in Falcon's `cars` table BY car id (the stable PK),
-- never by plate string. `petroapp_plate` is kept for human reference only.
CREATE TABLE IF NOT EXISTS petroapp_vehicle_map (
    id                  BIGSERIAL    PRIMARY KEY,
    petroapp_vehicle_id BIGINT       NOT NULL,
    petroapp_plate      TEXT,                         -- reference only
    db_car_id           BIGINT       NOT NULL REFERENCES cars (id),
    active              BOOLEAN      NOT NULL DEFAULT TRUE,
    created_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ  NOT NULL DEFAULT now()
);
-- One active mapping per PetroApp vehicle and per car; inactive history allowed.
CREATE UNIQUE INDEX IF NOT EXISTS uq_vmap_petroapp_vehicle_active
    ON petroapp_vehicle_map (petroapp_vehicle_id) WHERE active;
CREATE UNIQUE INDEX IF NOT EXISTS uq_vmap_car_active
    ON petroapp_vehicle_map (db_car_id) WHERE active;

-- Raw bills: the service's own JSONB landing table, keyed by the PetroApp bill id.
CREATE TABLE IF NOT EXISTS petroapp_raw_bills (
    petroapp_id    BIGINT       PRIMARY KEY,
    payload        JSONB        NOT NULL,
    vehicle_plate  TEXT,
    processed      BOOLEAN      NOT NULL DEFAULT FALSE,
    process_error  TEXT,
    fetched_at     TIMESTAMPTZ  NOT NULL DEFAULT now(),
    processed_at   TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_raw_bills_unprocessed
    ON petroapp_raw_bills (processed) WHERE processed = FALSE;

-- Sync runs: one row per cycle, for observability.
CREATE TABLE IF NOT EXISTS petroapp_sync_runs (
    id                BIGSERIAL    PRIMARY KEY,
    trigger           TEXT         NOT NULL DEFAULT 'schedule',
    status            TEXT         NOT NULL DEFAULT 'running',
    pages_fetched     INTEGER      NOT NULL DEFAULT 0,
    bills_fetched     INTEGER      NOT NULL DEFAULT 0,
    events_inserted   INTEGER      NOT NULL DEFAULT 0,
    unmapped_count    INTEGER      NOT NULL DEFAULT 0,
    anomaly_count     INTEGER      NOT NULL DEFAULT 0,
    error             TEXT,
    started_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    finished_at       TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_sync_runs_started ON petroapp_sync_runs (started_at DESC);
