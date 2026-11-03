import { createWriteStream } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { readCandles } from "../storage/candles.js";
import { getCoverage } from "../storage/coverage.js";

const MINUTE_MS = 60_000;

const CSV_COLUMNS = [
  "exchange",
  "category",
  "symbol",
  "openTimeUnix",
  "open",
  "high",
  "low",
  "close",
  "volume",
  "quoteTurnover",
] as const;

export interface MarketExportManifestInstrument {
  exchange: string;
  category: string;
  symbol: string;
  status: string;
  firstMinuteUnix: number | null;
  lastMinuteUnix: number | null;
  totalCandles: number;
}

export interface MarketExportManifest {
  formatVersion: 1;
  generatedAt: string;
  time: {
    semantics: "unix-seconds-utc";
    column: "openTimeUnix";
    meaning: "minute open time";
  };
  units: {
    open: "decimal-string";
    high: "decimal-string";
    low: "decimal-string";
    close: "decimal-string";
    volume: "decimal-string-base-asset";
    quoteTurnover: "decimal-string-quote-asset";
  };
  csvColumns: string[];
  instruments: MarketExportManifestInstrument[];
}

const unixOf = (iso: string | null): number | null =>
  iso === null ? null : Math.floor(new Date(iso).getTime() / 1000);

export async function exportMarketData(
  db: Kysely<MarketDataDB>,
  outDir: string,
): Promise<MarketExportManifest> {
  await mkdir(outDir, { recursive: true });

  const instruments = await db
    .selectFrom("instruments")
    .innerJoin("exchanges", "exchanges.id", "instruments.exchange_id")
    .select([
      "instruments.id as id",
      "exchanges.code as exchange",
      "instruments.category as category",
      "instruments.symbol as symbol",
      "instruments.status as status",
    ])
    .orderBy("exchanges.code", "asc")
    .orderBy("instruments.symbol", "asc")
    .execute();

  const stream = createWriteStream(`${outDir}/candles.csv`, { encoding: "utf8" });
  stream.write(`${CSV_COLUMNS.join(",")}\n`);
  const manifestInstruments: MarketExportManifestInstrument[] = [];

  for (const instrument of instruments) {
    const coverage = await getCoverage(db, instrument.id);
    const entry: MarketExportManifestInstrument = {
      exchange: instrument.exchange,
      category: instrument.category,
      symbol: instrument.symbol,
      status: instrument.status,
      firstMinuteUnix: unixOf(coverage.firstMinute),
      lastMinuteUnix: unixOf(coverage.lastMinute),
      totalCandles: 0,
    };

    if (coverage.firstMinute !== null && coverage.lastMinute !== null) {
      const from = new Date(coverage.firstMinute);
      const to = new Date(new Date(coverage.lastMinute).getTime() + MINUTE_MS);
      let afterUnix: number | undefined;
      for (;;) {
        const page = await readCandles(db, {
          instrumentId: instrument.id,
          from,
          to,
          limit: 5000,
          afterUnix,
        });
        for (const candle of page.candles) {
          stream.write(
            `${instrument.exchange},${instrument.category},${instrument.symbol},` +
              `${candle.openTimeUnix},${candle.open},${candle.high},${candle.low},` +
              `${candle.close},${candle.volume},${candle.quoteTurnover}\n`,
          );
        }
        entry.totalCandles += page.candles.length;
        if (page.nextCursorUnix === null) {
          break;
        }
        afterUnix = page.nextCursorUnix;
      }
    }
    manifestInstruments.push(entry);
  }

  await new Promise<void>((resolve, reject) => {
    stream.end((error?: Error | null) => {
      if (error) {
        reject(error);
      } else {
        resolve();
      }
    });
  });

  const manifest: MarketExportManifest = {
    formatVersion: 1,
    generatedAt: new Date().toISOString(),
    time: {
      semantics: "unix-seconds-utc",
      column: "openTimeUnix",
      meaning: "minute open time",
    },
    units: {
      open: "decimal-string",
      high: "decimal-string",
      low: "decimal-string",
      close: "decimal-string",
      volume: "decimal-string-base-asset",
      quoteTurnover: "decimal-string-quote-asset",
    },
    csvColumns: [...CSV_COLUMNS],
    instruments: manifestInstruments,
  };

  await writeFile(`${outDir}/manifest.json`, JSON.stringify(manifest, null, 2), "utf8");
  return manifest;
}
