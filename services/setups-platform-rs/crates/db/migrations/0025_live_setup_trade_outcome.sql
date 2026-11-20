ALTER TABLE public.live_setups ADD COLUMN trade_outcome jsonb;
ALTER TABLE public.live_setups ADD CONSTRAINT live_setup_trade_outcome_object CHECK (
    trade_outcome IS NULL OR (
        jsonb_typeof(trade_outcome) = 'object'
        AND trade_outcome->>'status' IS NOT NULL
        AND trade_outcome->>'status' IN ('tp','sl','gate_failed','unclosed','no_data')
        AND pg_column_size(trade_outcome) <= 4096
    )
);
CREATE INDEX live_setups_outcome_refresh ON public.live_setups(id)
WHERE adaptive_slot_id IS NOT NULL AND recommendation IS NOT NULL
  AND (trade_outcome IS NULL OR trade_outcome->>'status' IN ('unclosed','no_data'));
