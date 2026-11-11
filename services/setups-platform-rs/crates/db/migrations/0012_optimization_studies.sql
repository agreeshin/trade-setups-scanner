CREATE TABLE public.optimization_studies (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    algorithm_id text NOT NULL,
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'cancel_requested', 'completed', 'cancelled', 'failed')),
    spec jsonb NOT NULL CHECK (octet_length(spec::text) <= 524288),
    progress jsonb NOT NULL CHECK (octet_length(progress::text) <= 8192),
    result jsonb CHECK (octet_length(result::text) <= 8388608),
    error jsonb CHECK (octet_length(error::text) <= 4096),
    events jsonb NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(events) = 'array' AND jsonb_array_length(events) <= 64
               AND octet_length(events::text) <= 65536),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (status IN ('completed', 'cancelled', 'failed') OR result IS NULL)
);

CREATE UNIQUE INDEX optimization_studies_one_active
    ON public.optimization_studies ((true))
    WHERE status IN ('queued', 'running', 'cancel_requested');
CREATE INDEX optimization_studies_algorithm_recent
    ON public.optimization_studies (algorithm_id, id DESC);
