import "dotenv/config";
import { createDb } from "../db/db.js";
import { exportMarketData } from "./market-export.js";

const outDir = process.argv[2];
if (outDir === undefined || outDir.trim() === "") {
  console.error("usage: tsx src/export/cli.ts <output-directory>");
  process.exit(1);
}

const databaseUrl = process.env.DATABASE_URL;
if (!databaseUrl) {
  console.error("DATABASE_URL is required");
  process.exit(1);
}

const db = createDb(databaseUrl);
try {
  const manifest = await exportMarketData(db, outDir);
  console.log(
    `exported ${manifest.instruments.length} instruments ` +
      `(${manifest.instruments.reduce((sum, item) => sum + item.totalCandles, 0)} candles) to ${outDir}`,
  );
} finally {
  await db.destroy();
}
