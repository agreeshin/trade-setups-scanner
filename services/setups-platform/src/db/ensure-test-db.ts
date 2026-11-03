import "dotenv/config";
import { Pool } from "pg";

const testUrl = process.env.TEST_DATABASE_URL;
if (!testUrl) {
  throw new Error("TEST_DATABASE_URL is required");
}

const adminUrl = new URL(testUrl);
adminUrl.pathname = "/postgres";

const pool = new Pool({ connectionString: adminUrl.toString() });
try {
  await pool.query("CREATE DATABASE signals_test");
  console.log("signals_test created");
} catch (error) {
  const code = (error as { code?: string }).code;
  if (code === "42P04") {
    console.log("signals_test already exists");
  } else {
    throw error;
  }
} finally {
  await pool.end();
}
