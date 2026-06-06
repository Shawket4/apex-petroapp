-- Seed the vehicle map from the legacy Go `PlateNumberMap`. The internal link is
-- resolved to cars.id BY MATCHING the Arabic plate against the live `cars` table,
-- so a plate that doesn't match a real car is simply not seeded (and warned about
-- below) rather than creating a dangling mapping.

INSERT INTO petroapp_vehicle_map (petroapp_vehicle_id, petroapp_plate, db_car_id)
SELECT v.petroapp_vehicle_id, v.petroapp_plate, c.id
FROM (
    VALUES
        (53008, 'C E F-4 3 8 1', 'ف ع ص 4381'),
        (53005, 'C Q F-4 2 5 3', 'ف ق ص 4253'),
        (53004, 'R Y F-9 1 5 6', 'ف ى ر 9156'),
        (53007, 'N A F-5 1 3 9', 'ف أ ن 5139'),
        (53010, 'S M F-9 2 4 7', 'ف م س 9247'),
        (53012, 'S R F-4 5 9 3', 'ف ر س 4593'),
        (53006, 'Y D F-6 5 8 4', 'ف د ى 6584'),
        (53011, 'Y D F-6 8 3 4', 'ف د ى 6834'),
        (53009, 'N A F-7 4 2 1', 'ف ا ن 7421'),
        (60335, 'R Y F-8 4 2 7', 'ف ى ر 8427'),
        (60336, 'C R F-6 3 5 9', 'ف ر ص 6359'),
        (53013, 'Y D F-6 7 3 8', 'ف د ى 6738')
) AS v (petroapp_vehicle_id, petroapp_plate, db_plate)
JOIN cars c ON c.car_no_plate = v.db_plate AND c.deleted_at IS NULL
ON CONFLICT DO NOTHING;

-- Warn (don't fail) about any legacy plate that didn't resolve to a live car, so
-- the operator can fix the mapping via POST /vehicle-map after deploy.
DO $$
DECLARE
    missing TEXT;
BEGIN
    SELECT string_agg(v.db_plate, ', ')
      INTO missing
    FROM (
        VALUES
            ('ف ع ص 4381'),('ف ق ص 4253'),('ف ى ر 9156'),('ف أ ن 5139'),
            ('ف م س 9247'),('ف ر س 4593'),('ف د ى 6584'),('ف د ى 6834'),
            ('ف ا ن 7421'),('ف ى ر 8427'),('ف ر ص 6359'),('ف د ى 6738')
    ) AS v (db_plate)
    WHERE NOT EXISTS (
        SELECT 1 FROM cars c
        WHERE c.car_no_plate = v.db_plate AND c.deleted_at IS NULL
    );

    IF missing IS NOT NULL THEN
        RAISE WARNING 'petroapp_vehicle_map seed: these plates did not match any car and were NOT seeded: %', missing;
    END IF;
END $$;
