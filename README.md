# apex-petroapp

A small, robust Rust microservice that replaces Falcon's Go PetroApp integration.
It owns fuel-bill ingestion, the credential store, the vehicle map, and the
odometer push-back hook, consuming the shared `apex` Postgres database directly.

## What it does

- **Ingestion (scheduled):** pulls PetroApp `bills`, lands them as JSONB, and
  converts them into `fuel_events` via one SQL pass (vehicle-map join on numeric
  `vehicle_id`, odometer baseline from your own history, `price_per_liter`,
  driver/transporter resolved from `cars`/`drivers`). No fabricated baselines, no
  "Unknown Driver" poison, exact dedup on `petroapp_bill_id`.
- **Dynamic fetch window:** each run fetches from *(latest stored bill date − 1 day)*
  through today, so the most recent day is always re-covered and gaps can't form.
  Falls back to `SYNC_LOOKBACK_DAYS` when there's no history yet.
- **Self-healing late mappings:** a bill for an unmapped vehicle parks itself
  (flagged, not dropped) and is ingested automatically once you add the mapping.
  If it lands chronologically *between* two existing events, the following event's
  `odometer_before`/`fuel_rate` are recomputed — only PetroApp-sourced rows are
  rewritten, so manual events are never disturbed.
- **Odometer push-back:** Falcon calls `POST /odometer/push` on manual fuel events.
- **Loud auth handling:** an expired token marks credentials `expired`, alerts
  (Slack + WhatsApp), and pauses ingestion instead of failing silently.
- **Observability:** every cycle is recorded in `petroapp_sync_runs`.

## Setup

```bash
cp .env.example .env      # fill in DATABASE_URL, INTERNAL_API_KEY, WhatsApp, etc.
cargo build --release
./target/release/apex-petroapp
```

Migrations (including the vehicle-map seed from the legacy `PlateNumberMap`) run
automatically on startup. The seed resolves each car id from your live `cars` table
by plate and **warns** (in the log) about any legacy plate that didn't match a car.

## Endpoints

Mutating endpoints require the `X-Api-Key` header (= `INTERNAL_API_KEY`).

| Method | Path | Purpose |
|---|---|---|
| GET  | `/health` | liveness + DB ping |
| GET  | `/status` | credential status + last run |
| POST | `/token` | set/replace token+cookie (the manual-login paste target) |
| POST | `/sync/trigger` | run a sync now |
| GET  | `/sync/runs?limit=` | recent run audit |
| GET  | `/vehicle-map` | list mappings |
| POST | `/vehicle-map` | add/update a mapping (`{petroapp_vehicle_id, db_car_id, petroapp_plate?, active?}`) |
| DELETE | `/vehicle-map/{petroapp_vehicle_id}` | deactivate a mapping |
| POST | `/odometer/push` | push-back hook (`{db_car_id, odometer}`) |

## Notes / confirm on first run

- `fuel_events.date` is treated as ISO `YYYY-MM-DD` and `time` as Go's `"3:04 PM"`
  (reproduced via `to_char(ts,'FMHH12:MI AM')`), matching Falcon's `AddFuelEvent`.
- The service is a **second writer** to `fuel_events`/`cars` alongside Falcon's
  manual flows; ordering/dedup/`GREATEST` guards are designed around that.
- A physical fill entered both manually and via PetroApp will produce two rows
  (manual events are not deduped against PetroApp bills) — expected.
