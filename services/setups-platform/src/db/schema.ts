import type { Generated } from "kysely";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";

export interface ReplayRunsTable {
  id: Generated<number>;
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

export interface ReplayCacheTable {
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
  replay_runs: ReplayRunsTable;
  replay_cache: ReplayCacheTable;
  settings: SettingsTable;
  live_setups: LiveSetupsTable;
  instrument_lists: InstrumentListsTable;
  kysely_migration: KyselyMigrationsTable;
}
