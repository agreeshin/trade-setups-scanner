-- Operational live metadata is separate from immutable research spec/result.
ALTER TABLE continuous_generations ADD COLUMN live_initialization jsonb
    CHECK (octet_length(live_initialization::text) <= 1024);
ALTER TABLE continuous_generations ADD COLUMN live_readiness jsonb NOT NULL DEFAULT '[]'
    CHECK (jsonb_typeof(live_readiness) = 'array' AND jsonb_array_length(live_readiness) <= 1000
        AND octet_length(live_readiness::text) <= 1048576);
ALTER TABLE continuous_generations ADD COLUMN live_init_attempts integer NOT NULL DEFAULT 0
    CHECK (live_init_attempts BETWEEN 0 AND 3);
ALTER TABLE continuous_generations ADD COLUMN live_init_status text
    CHECK (live_init_status IN ('running','retry','exhausted','active','superseded'));
ALTER TABLE continuous_generations ADD COLUMN live_init_deadline timestamptz;
ALTER TABLE continuous_generations ADD COLUMN live_init_next_attempt_at timestamptz;
ALTER TABLE continuous_generations ADD COLUMN live_init_error jsonb
    CHECK (octet_length(live_init_error::text) <= 4096);

-- Legacy incumbents keep their original deterministic rebuild boundary. Do not
-- choose now(), overwrite research/bootstrap, or move retained detector cursors.
UPDATE continuous_generations g SET live_initialization=jsonb_build_object(
    'version',1,
    'anchorCloseUnix',coalesce((result#>>'{bootstrap,observationCutoff}')::bigint,observation_cutoff),
    'detectorFromOpenUnix',coalesce((result#>>'{bootstrap,detectorWarmupFromUnix}')::bigint,
        observation_cutoff-((coalesce((result#>>'{best,params,baselineLongDays}')::bigint,30)*1440
        +coalesce((result#>>'{best,params,windowMinutes}')::bigint,5)+1)*60)),
    'emitFromCloseUnix',coalesce((result#>>'{bootstrap,emitFromCloseUnix}')::bigint,observation_cutoff+60)),
    live_init_status='active'
WHERE EXISTS(SELECT 1 FROM continuous_slots s WHERE s.active_generation_id=g.id);

-- Existing durable cursors are legacy publication authority, not a new claim
-- that every declared instrument has data. Absent cursors stay explicitly
-- waiting/unknown; workers retain each incumbent's own rebuild origin.
UPDATE continuous_generations g SET live_readiness=(
    SELECT coalesce(jsonb_agg(jsonb_build_object(
        'instrumentKey',(i->>'exchange')||'/'||(i->>'category')||'/'||(i->>'symbol'),
        'status',CASE WHEN c.instrument_key IS NULL THEN 'waiting' ELSE 'ready' END,
        'reason',CASE WHEN c.instrument_key IS NULL THEN 'unknown' ELSE NULL END,
        'publicationFloorCloseUnix',coalesce(
            (SELECT s.live_watermark FROM continuous_slots s WHERE s.active_generation_id=g.id LIMIT 1),
            (g.live_initialization->>'anchorCloseUnix')::bigint))
        ORDER BY i->>'exchange',i->>'category',i->>'symbol'),'[]'::jsonb)
    FROM jsonb_array_elements(g.spec->'instruments') i
    LEFT JOIN adaptive_live_checkpoints c ON c.slot_id=g.slot_id AND c.generation_id=g.id
        AND c.instrument_key=(i->>'exchange')||'/'||(i->>'category')||'/'||(i->>'symbol'))
WHERE EXISTS(SELECT 1 FROM continuous_slots s WHERE s.active_generation_id=g.id);
