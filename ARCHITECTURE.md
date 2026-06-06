# apex-petroapp — Architecture Plan

A Rust microservice that replaces Falcon's Go PetroApp integration. It owns fuel-bill
ingestion, the credential store, the vehicle map, and the odometer push-back hook,
consuming the shared `apex` Postgres database directly. Modeled on the etit-proxy
pattern: a small, robust, single-purpose service that owns its slice of behavior.

This plan is grounded in the real GORM schema and a representative bills payload, not
inferred shapes. A handful of confirmations remain (end of doc) — the date/time string
format is the only one that blocks building.

---

## 1. Ownership boundary

**Tables the service owns** (new, no other writer):

- `petroapp_credentials` — the manually-captured token + cookie, single row, with a
  `status` field (`valid` / `expired` / `unknown`). Replaces Falcon's `petro_app_tokens`.
- `petroapp_vehicle_map` — replaces the hardcoded Go `PlateNumberMap`. Holds
  `petroapp_vehicle_id` (the join key), `petroapp_plate` (reference), and `db_car_plate`.
  Adding a truck is now an INSERT.
- `petroapp_raw_bills` — the raw landing table, storing each bill as JSONB keyed by the
  PetroApp bill id. We use our own JSONB table rather than Falcon's wide
  `petro_app_records` (which maps 40+ columns, several as strings) so the service is
  decoupled and Falcon can drop `petro_app_records` entirely.
- `petroapp_sync_runs` — one row per sync cycle: counts, status, error, timing. Replaces
  the old "swallow the error, retry in 5 min, fail silently" behavior with a durable audit.

**Falcon tables the service touches** (carefully, as a second writer):

- `fuel_events` — INSERT only. Never updates or deletes. Respects soft-deletes
  (`deleted_at IS NULL`) on all reads. New events are tagged with `petroapp_bill_id`.
- `cars` — READ (car id, transporter, assigned driver) and a guarded UPDATE of
  `last_fuel_odometer`.
- `drivers` — READ only (resolve `name` via `cars.driver_id`).

**Decision: direct DB writes, not calls into Falcon's Go API.** The writes are
well-defined inserts, and the second-writer hazards (below) are handled at the DB level
with `GREATEST` guards, soft-delete filters, and exact `petroapp_bill_id` dedup. Routing
through Falcon would reintroduce the Go code we're trying to retire. The service is the
authoritative writer for PetroApp-sourced events; Falcon remains authoritative for manual
events. They coexist via the `petroapp_bill_id` tag.

---

## 2. Second-writer hazards and how they're handled

The report confirmed that manual `AddFuelEvent` / `EditFuelEvent` also write
`fuel_events.{fuel_rate, odometer_before, odometer_after}` and update
`cars.last_fuel_odometer`. Consequences baked into the design:

- **Dedup keys on `petroapp_bill_id`, not the fuzzy tuple.** A new nullable
  `petroapp_bill_id` column on `fuel_events` with a partial unique index
  (`WHERE petroapp_bill_id IS NOT NULL`) gives exact one-event-per-bill semantics.
  Manual events leave it NULL and never collide.
- **`cars.last_fuel_odometer` is updated with `GREATEST`**, so ingestion can't roll back a
  more-recent reading a manual event just wrote.
- **Previous-odometer lookup intentionally includes manual events.** They're real
  readings, so the baseline for efficiency should see them. Reads filter `deleted_at IS NULL`.
- **Open question — same physical fill entered both ways.** If an operator manually logs a
  fill that also comes through PetroApp, both rows will exist (double count), because
  dedup only matches PetroApp rows to each other. This is probably fine (manual events are
  typically non-PetroApp/cash fills), but it's a confirmation item below.

---

## 3. Schema migrations

**Service-owned tables** (`0001`): `petroapp_credentials`, `petroapp_vehicle_map`,
`petroapp_raw_bills` (JSONB), `petroapp_sync_runs`, with the appropriate unique and
partial indexes.

**Additive change to `fuel_events`** (`0002`):

```sql
ALTER TABLE fuel_events ADD COLUMN IF NOT EXISTS petroapp_bill_id BIGINT;
CREATE UNIQUE INDEX IF NOT EXISTS uq_fuel_events_petroapp_bill_id
    ON fuel_events (petroapp_bill_id) WHERE petroapp_bill_id IS NOT NULL;
```

Non-destructive, ignored by GORM (extra column), and the partial index means manual events
(NULL) are unconstrained. Note: a soft-deleted PetroApp event still occupies the index, so
the same bill won't be re-created after a soft delete — intended.

---

## 4. The ingestion logic (SQL-owned)

The conversion from raw JSONB bills to `fuel_events` is one statement. All the
schema-driven specifics live here:

```sql
WITH unprocessed AS (
  SELECT
    b.petroapp_id,
    (b.payload->>'vehicle_id')::bigint                 AS petroapp_vehicle_id,
    (b.payload->>'odo')::int                           AS odo,
    NULLIF(b.payload->>'number_of_liters','')::numeric AS liters,  -- arrives as string
    NULLIF(b.payload->>'cost','')::numeric             AS cost,    -- arrives as string
    NULLIF(b.payload->>'delegate_name','')             AS delegate_name,
    -- PetroApp sends "YYYY-MM-DD HH:MM:SS"; split to match Falcon's date/time columns
    split_part(b.payload->>'date',' ',1)               AS bill_date,  -- CONFIRM format
    split_part(b.payload->>'date',' ',2)               AS bill_time
  FROM petroapp_raw_bills b
  WHERE b.processed = false
),
mapped AS (
  SELECT u.*, m.db_car_plate
  FROM unprocessed u
  JOIN petroapp_vehicle_map m            -- join on NUMERIC vehicle_id (robust)
    ON m.petroapp_vehicle_id = u.petroapp_vehicle_id AND m.active
),
resolved AS (
  SELECT mp.*, c.id AS car_id, c.transporter,
         COALESCE(NULLIF(d.name,''), mp.delegate_name, '') AS driver_name
  FROM mapped mp
  JOIN cars c    ON c.car_no_plate = mp.db_car_plate AND c.deleted_at IS NULL
  LEFT JOIN drivers d ON d.id = c.driver_id AND d.deleted_at IS NULL
),
derived AS (
  SELECT r.*,
    (SELECT fe.odometer_after FROM fuel_events fe
      WHERE fe.car_no_plate = r.db_car_plate AND fe.deleted_at IS NULL
        AND fe.date < r.bill_date
      ORDER BY fe.date DESC, fe.odometer_after DESC LIMIT 1) AS prev_odo
  FROM resolved r
)
INSERT INTO fuel_events
  (car_id, car_no_plate, driver_name, date, time, liters, price_per_liter, price,
   fuel_rate, transporter, odometer_before, odometer_after, method,
   petroapp_bill_id, created_at, updated_at)
SELECT
  d.car_id, d.db_car_plate, d.driver_name, d.bill_date, d.bill_time,
  COALESCE(d.liters,0),
  CASE WHEN COALESCE(d.liters,0) > 0 THEN d.cost / d.liters ELSE 0 END,
  COALESCE(d.cost,0),
  CASE
    WHEN d.prev_odo IS NULL          THEN 0   -- no baseline (flagged, not fabricated)
    WHEN (d.odo - d.prev_odo) <= 0   THEN 0   -- non-positive delta: don't invent
    WHEN COALESCE(d.liters,0) <= 0   THEN 0
    ELSE (d.odo - d.prev_odo)::numeric / d.liters
  END,
  d.transporter,
  COALESCE(d.prev_odo, d.odo),               -- before = prev, or self when no baseline
  d.odo, 'PetroApp', d.petroapp_id, now(), now()
FROM derived d
ON CONFLICT (petroapp_bill_id) WHERE petroapp_bill_id IS NOT NULL DO NOTHING
RETURNING car_no_plate, date, odometer_after, liters, fuel_rate, petroapp_bill_id;
```

Then, in the same transaction: mark every *mapped* unprocessed bill `processed = true`,
flag the remainder with `process_error = 'no active vehicle mapping'` (left unprocessed so
they're picked up automatically once the map is updated), and update `cars`:

```sql
UPDATE cars c
SET last_fuel_odometer = GREATEST(c.last_fuel_odometer, sub.max_odo), updated_at = now()
FROM (SELECT car_no_plate, MAX(odometer_after) max_odo
        FROM fuel_events WHERE petroapp_bill_id = ANY($1) GROUP BY car_no_plate) sub
WHERE c.car_no_plate = sub.car_no_plate AND c.deleted_at IS NULL;
```

**Improvements over the Go version, all visible here:**

- Join on numeric `vehicle_id` instead of the spaced plate string — no whitespace/format
  fragility.
- No fabricated `current − 500` baseline. First event with no prior reading gets
  `odometer_before = odometer_after` (delta 0, `fuel_rate` 0) and is surfaced in the run
  summary as baseline-pending, rather than silently corrupting consumption stats.
- No `"PetroApp Unknown Driver"` poison string — driver falls back to delegate name, then
  empty string.
- `price_per_liter` populated; `car_id`, `transporter` resolved from the `cars` source of
  truth rather than duplicated config.
- Unmapped bills park themselves and auto-process when the map gets the missing vehicle.

---

## 5. PetroApp client behavior

- **Paginated bills pull** across `meta.last_page` (the Go version only read page 1).
- **Auth failures (401/403) are a distinct, non-retried signal** → mark
  `petroapp_credentials.status = 'expired'`, alert, abort the run cleanly. This kills the
  worst current fragility (silent death on expiry).
- **Bounded exponential backoff** on transient network / 5xx errors.
- **`edit_odometer` push-back**: `POST /edit_odometer`, body `{"vehicle_id": <int>,
  "odometer": <int>}`, `Authorization` + `Cookie` headers — matching the captured request
  exactly.

---

## 6. HTTP API (the Falcon contract)

Mutating endpoints require an `X-Api-Key` header matching `INTERNAL_API_KEY`.

- `POST /odometer/push` — the migrated hook. Body `{ "db_car_plate": "...", "odometer":
  <int> }` (or `db_car_id`). Resolves `petroapp_vehicle_id` from the map and calls
  `edit_odometer`. **Falcon's `AddFuelEvent` / `EditFuelEvent` call this instead of doing
  the push inline.**
- `POST /token` — set/replace credentials (the paste target for the manual login flow).
- `GET /status` — credential status + last sync run, for a health glance.
- `GET /health` — liveness + DB ping.
- `POST /sync/trigger` — run a sync now; returns the run id.
- `GET /sync/runs?limit=` — recent run audit.
- `GET /vehicle-map`, `POST /vehicle-map`, `DELETE /vehicle-map/{petroapp_vehicle_id}` —
  manage the mapping without redeploying.

---

## 7. Notifications

To preserve the current WhatsApp efficiency messages, the service POSTs to the existing
internal WhatsApp Go service (`<WHATSAPP_SERVICE_URL>/send/message`, body
`{"phone","message"}`). Anomaly band (`fuel_rate` outside ~1.9–2.8) stays, configurable.
A Slack-compatible webhook is also supported for operational alerts (token expired,
unmapped vehicles, run failures). Notification failures never fail a sync — they log.

---

## 8. Robustness features

Token health tracking with expired-state + alert; per-run audit table; transactional
conversion; exact idempotent dedup; bounded retries with backoff; soft-delete-aware reads;
`GREATEST`-guarded `cars` update; startup config validation; API-key guard on mutating
routes; structured `tracing` logs; a scheduler that records and survives per-cycle failures
instead of dying silently.

---

## 9. Falcon-side changes (for the later removal prompt)

**Keep nothing PetroApp-specific in Falcon except the call-out.** Specifically:

- `AddFuelEvent` / `EditFuelEvent` lose their inline `UpdatePetroAppOdometerFromManualFuelEvent`
  call and instead fire `POST /odometer/push` to this service (best-effort, non-blocking, so
  a push-back failure never blocks saving a manual event — just logs/alerts).
- Delete: the 5-minute ticker (`petroAppSyncWorker`), `FetchPetroAppRecords`,
  `StoreUniquePetroAppRecords`, `SyncPetroAppRecordsToFuelEvents`,
  `getPreviousOdometerReading`, `UpdatePetroAppOdometerFromManualFuelEvent`,
  `convertPlateNumber`, the `PlateNumberMap`, `SetPetroAppTokens` + its route, and the
  `PetroAppToken` / `PetroAppRecord` models + their tables (after data is no longer needed).
- The vehicle map and credentials now live in this service; the manual login paste targets
  `POST /token` here instead of Falcon.

---

## 10. Remaining confirmations before building

1. **Date/time string format in `fuel_events` (blocking).** I'm assuming Falcon stores
   `date` as ISO `YYYY-MM-DD` and `time` as `HH:MM:SS`, produced by splitting PetroApp's
   `"YYYY-MM-DD HH:MM:SS"` on the space. This matters because the previous-odometer lookup
   orders by the `date` *string* — it's only chronologically correct if the format is ISO.
   Confirm by showing how `AddFuelEvent` builds `date` and `time`.
2. **WhatsApp recipients + service URL.** What is `WhatsappGoService`'s base URL, and which
   phone number(s) should efficiency/anomaly messages go to? Keep per-event messages, or
   only send on anomalies?
3. **Manual-vs-PetroApp same-fill duplication** (section 2). Acceptable, or should the
   service also skip a PetroApp event that a manual event already covers?
4. **Join-key switch.** Confirm it's fine to match on numeric `vehicle_id` rather than the
   plate string (more robust; the map stores both).
5. **Nullability sanity.** I'll write concrete values for `odometer_before`, `driver_name`,
   `fuel_rate` (treating them as NOT NULL, which is safe either way) — flag if any are
   actually nullable and you'd prefer NULL for "unknown".
