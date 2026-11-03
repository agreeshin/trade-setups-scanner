import type { Generated } from "kysely";

export interface ExchangesTable {
  id: Generated<number>;
  code: string;
  name: string;
  created_at: Generated<Date>;
}

export type InstrumentStatus = "active" | "delisted";

export interface InstrumentsTable {
  id: Generated<number>;
  exchange_id: number;
  category: string;
  symbol: string;
  status: InstrumentStatus;
  listed_at: Date | null;
  delisted_at: Date | null;
  created_at: Generated<Date>;
}

export interface CandlesTable {
  instrument_id: number;
  open_time: Date;
  open: string;
  high: string;
  low: string;
  close: string;
  volume: string;
  quote_turnover: string;
  source: string;
  ingested_at: Generated<Date>;
}

export interface CandleConflictsTable {
  id: Generated<number>;
  instrument_id: number;
  open_time: Date;
  existing_record: CandleRecordJson;
  incoming_record: CandleRecordJson;
  detected_at: Generated<Date>;
}

export interface CandleRecordJson {
  open: string;
  high: string;
  low: string;
  close: string;
  volume: string;
  quote_turnover: string;
}

export interface KyselyMigrationsTable {
  name: string;
  timestamp: string;
}

export type BackfillJobStatus =
  | "queued"
  | "running"
  | "done"
  | "cancel_requested"
  | "cancelled"
  | "failed";

export interface BackfillJobsTable {
  id: Generated<number>;
  instrument_id: number;
  from_ts: Date;
  to_ts: Date;
  status: BackfillJobStatus;
  checkpoint_ts: Date | null;
  requested_by: string[];
  error: string | null;
  created_at: Generated<Date>;
  updated_at: Generated<Date>;
}

export interface ServiceStateValue {
  cooldownUntilIso: string | null;
}

export interface ServiceStateTable {
  key: string;
  value: ServiceStateValue;
  updated_at: Generated<Date>;
}

export interface CollectionTargetsTable {
  instrument_id: number;
  enabled: boolean;
  last_collected_ts: Date | null;
  updated_at: Generated<Date>;
}

export interface MarketDataDB {
  exchanges: ExchangesTable;
  instruments: InstrumentsTable;
  candles: CandlesTable;
  candle_conflicts: CandleConflictsTable;
  backfill_jobs: BackfillJobsTable;
  service_state: ServiceStateTable;
  collection_targets: CollectionTargetsTable;
  kysely_migration: KyselyMigrationsTable;
}
