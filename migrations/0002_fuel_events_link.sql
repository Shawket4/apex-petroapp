-- Additive, non-destructive change to Falcon's GORM-managed `fuel_events` table.
-- Adds a nullable link to the source PetroApp bill so ingestion dedup is exact
-- (one fuel event per bill) instead of relying on the fuzzy
-- (car_no_plate, date, odometer_before, odometer_after) heuristic, which breaks
-- once odometer_before is allowed to be NULL.
--
-- Manual fuel events leave petroapp_bill_id NULL; the partial unique index only
-- constrains rows that came from PetroApp, so NULLs never collide.

ALTER TABLE fuel_events
    ADD COLUMN IF NOT EXISTS petroapp_bill_id BIGINT;

CREATE UNIQUE INDEX IF NOT EXISTS uq_fuel_events_petroapp_bill_id
    ON fuel_events (petroapp_bill_id)
    WHERE petroapp_bill_id IS NOT NULL;
