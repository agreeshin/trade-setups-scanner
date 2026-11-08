import type { Generated } from "kysely";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";
import type { QualityReason, ReplayQuality } from "../replay/quality.js";

export interface ReplayRunsTable {
  quality: Generated<ReplayQuality | null>;
  quality_source_run_id: Generated<number | null>;
  quality_computed_at: Generated<Date | null>;
  id: Generated<number>;
  batch_id: number | null;
  exchange_code: string;
  category: string;
  symbol: string;
  from_ts: Date;
  to_ts: Date;
  algorithm_id: string;
  algorithm_version: string;
  params: SpikeParams;
  status: string;
  cache_hit: boolean;
  setups: SpikeSetup[] | null;
  warmup_minutes: number | null;
  processed_minutes: number | null;
  error: string | null;
  created_at: Generated<Date>;
  updated_at: Generated<Date>;
}

export interface ReplayBatchesTable {
  id: Generated<number>;
  exchange_code: string;
  category: string;
  algorithm_id: string;
  algorithm_version: string | null;
  params: SpikeParams;
  from_ts: Date;
  to_ts: Date;
  instrument_count: number;
  created_at: Generated<Date>;
}

export interface ReplayCacheTable {
  quality: Generated<ReplayQuality | null>;
  quality_source_run_id: Generated<number | null>;
  cache_key: string;
  exchange_code: string;
  category: string;
  symbol: string;
  algorithm_id: string;
  algorithm_version: string;
  params: SpikeParams;
  from_ts: Date;
  to_ts: Date;
  setups: SpikeSetup[];
  warmup_minutes: number;
  processed_minutes: number;
  computed_at: Generated<Date>;
}

export interface SettingsTable {
  key: string;
  value: Record<string, unknown>;
  updated_at: Generated<Date>;
}

export interface LiveSetupsTable {
  id: Generated<number>;
  exchange_code: string;
  category: string;
  symbol: string;
  algorithm_id: string;
  algorithm_version: string;
  params: SpikeParams;
  open_time: Date;
  close_time: Date;
  close_price: string;
  details: Generated<NonNullable<SpikeSetup["details"]> | null>;
  notified_at: Date | null;
  created_at: Generated<Date>;
}

export interface InstrumentListsTable {
  id: Generated<number>;
  list_type: string;
  exchange_code: string;
  category: string;
  symbol: string;
  created_at: Generated<Date>;
}

export interface KyselyMigrationsTable {
  name: string;
  timestamp: string;
}

export interface SignalsDB {
  replay_quality_intervals: { run_id: number; from_unix: number; to_unix: number; reason: QualityReason };
  replay_runs: ReplayRunsTable;
  replay_batches: ReplayBatchesTable;
  replay_cache: ReplayCacheTable;
  settings: SettingsTable;
  live_setups: LiveSetupsTable;
  instrument_lists: InstrumentListsTable;
  kysely_migration: KyselyMigrationsTable;
}
