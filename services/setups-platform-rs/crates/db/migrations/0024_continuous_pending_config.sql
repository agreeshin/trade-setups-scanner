-- Deferred research configuration: authorized saves while a continuous chain
-- may be in flight land in pending_config. The active generation keeps its
-- config/revision fences and finishes with the parameters it was launched
-- with; the pending config is applied atomically when the generation ends
-- (finalization/fail) or when a stopped process starts again.
ALTER TABLE continuous_processes ADD COLUMN IF NOT EXISTS pending_config jsonb;
