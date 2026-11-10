ALTER TABLE public.replay_runs ADD COLUMN IF NOT EXISTS name text;
ALTER TABLE public.replay_batches ADD COLUMN IF NOT EXISTS name text;
