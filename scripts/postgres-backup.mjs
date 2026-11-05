#!/usr/bin/env node
import { createHash, randomUUID } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { mkdir, open, rename, rm, stat, statfs, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { spawn } from "node:child_process";
import { Transform, Writable } from "node:stream";
import { pipeline } from "node:stream/promises";

const HEADER_BYTES = 4096;
const MAGIC = "TRADE_SETUPS_BACKUP_V1";
const VALID_SCOPES = new Set(["market-data", "platform"]);

function option(name) {
  const index = process.argv.indexOf(`--${name}`);
  if (index < 0 || !process.argv[index + 1]) throw new Error(`missing --${name}`);
  return process.argv[index + 1];
}

function databaseEnvironment() {
  const raw = process.env.DATABASE_URL;
  if (!raw) throw new Error("DATABASE_URL is required");
  const url = new URL(raw);
  return {
    ...process.env,
    PGHOST: url.hostname,
    PGPORT: url.port || "5432",
    PGUSER: decodeURIComponent(url.username),
    PGPASSWORD: decodeURIComponent(url.password),
    PGDATABASE: decodeURIComponent(url.pathname.slice(1)),
  };
}

async function run(command, args, { stdout } = {}) {
  const child = spawn(command, args, {
    env: databaseEnvironment(),
    stdio: ["ignore", stdout ? "pipe" : "ignore", "pipe"],
  });
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-4000); });
  const completion = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", resolve);
  });
  try {
    if (stdout) await pipeline(child.stdout, stdout);
  } catch (error) {
    child.kill();
    await completion.catch(() => undefined);
    throw error;
  }
  const code = await completion;
  if (code !== 0) throw new Error(`${command} failed (${code}): ${stderr.trim()}`);
}

async function capture(command, args) {
  let output = "";
  const sink = new Writable({
    write(chunk, _encoding, callback) {
      output += chunk.toString("utf8");
      callback();
    },
  });
  await run(command, args, { stdout: sink });
  return output.trim();
}

async function sha256(path, start = 0) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path, { start })) hash.update(chunk);
  return hash.digest("hex");
}

async function withLock(path, operation) {
  const handle = await open(path, "wx", 0o600).catch((error) => {
    if (error?.code === "EEXIST") throw new Error("backup operation already in progress");
    throw error;
  });
  try {
    return await operation();
  } finally {
    await handle.close();
    await rm(path, { force: true });
  }
}

async function schemaVersion() {
  try {
    return await capture("psql", ["--no-align", "--tuples-only", "--command", "select name from kysely_migration order by timestamp desc limit 1"]);
  } catch {
    return "unknown";
  }
}

async function createBackup(scope, output) {
  if (!VALID_SCOPES.has(scope)) throw new Error("invalid backup scope");
  await mkdir(dirname(output), { recursive: true, mode: 0o700 });
  await withLock(join(dirname(output), `${scope}.backup.lock`), async () => {
    const temporary = `${output}.${randomUUID()}.partial`;
    try {
      const handle = await open(temporary, "wx", 0o600);
      await handle.write(Buffer.alloc(HEADER_BYTES), 0, HEADER_BYTES, 0);
      await handle.close();
      await run("pg_dump", ["--format=custom", "--compress=6", "--no-owner", "--no-privileges"], {
        stdout: createWriteStream(temporary, { flags: "r+", start: HEADER_BYTES, mode: 0o600 }),
      });
      const file = await stat(temporary);
      const payloadBytes = file.size - HEADER_BYTES;
      const manifest = {
        format: MAGIC,
        scope,
        createdAt: new Date().toISOString(),
        schemaVersion: await schemaVersion(),
        postgresClient: await capture("pg_dump", ["--version"]),
        payloadBytes,
        payloadSha256: await sha256(temporary, HEADER_BYTES),
      };
      const header = Buffer.from(`${MAGIC}\n${JSON.stringify(manifest)}\n`, "utf8");
      if (header.length > HEADER_BYTES) throw new Error("backup manifest exceeds header budget");
      const padded = Buffer.alloc(HEADER_BYTES, 0x20);
      header.copy(padded);
      const outputHandle = await open(temporary, "r+");
      await outputHandle.write(padded, 0, HEADER_BYTES, 0);
      await outputHandle.sync();
      await outputHandle.close();
      await rename(temporary, output);
      process.stdout.write(`${JSON.stringify(manifest)}\n`);
    } catch (error) {
      await rm(temporary, { force: true });
      throw error;
    }
  });
}

async function readManifest(input) {
  const handle = await open(input, "r");
  const buffer = Buffer.alloc(HEADER_BYTES);
  const { bytesRead } = await handle.read(buffer, 0, HEADER_BYTES, 0);
  await handle.close();
  if (bytesRead !== HEADER_BYTES) throw new Error("backup file is truncated");
  const [magic, json] = buffer.toString("utf8").split("\n", 3);
  if (magic !== MAGIC || !json) throw new Error("unsupported backup format");
  const manifest = JSON.parse(json);
  if (manifest.format !== MAGIC || !VALID_SCOPES.has(manifest.scope)) throw new Error("invalid backup manifest");
  return manifest;
}

async function stageRestore(scope, input, backupDir) {
  if (!VALID_SCOPES.has(scope)) throw new Error("invalid backup scope");
  await mkdir(backupDir, { recursive: true, mode: 0o700 });
  await withLock(join(backupDir, `${scope}.restore.lock`), async () => {
    const marker = join(backupDir, `${scope}.restore.pending`);
    const dump = join(backupDir, `${scope}.restore.pending.dump`);
    const markerExists = await stat(marker).then(() => true, () => false);
    if (markerExists) throw new Error("a restore is already pending");
    const manifest = await readManifest(input);
    if (manifest.scope !== scope) throw new Error(`backup scope mismatch: expected ${scope}, got ${manifest.scope}`);
    const inputStat = await stat(input);
    if (inputStat.size !== HEADER_BYTES + manifest.payloadBytes) throw new Error("backup payload size mismatch");
    const fs = await statfs(backupDir);
    const databaseBytes = Number(await capture("psql", ["--no-align", "--tuples-only", "--command", "select pg_database_size(current_database())"]));
    const requiredBytes = manifest.payloadBytes + Math.ceil(databaseBytes * 1.2) + 64 * 1024 ** 2;
    const availableBytes = Number(fs.bavail) * Number(fs.bsize);
    if (!Number.isSafeInteger(requiredBytes) || availableBytes < requiredBytes) {
      throw new Error(`insufficient restore space: required ${requiredBytes}, available ${availableBytes}`);
    }
    const temporary = `${dump}.${randomUUID()}.partial`;
    const hash = createHash("sha256");
    const digest = new Transform({ transform(chunk, _encoding, callback) { hash.update(chunk); callback(null, chunk); } });
    try {
      await pipeline(
        createReadStream(input, { start: HEADER_BYTES }),
        digest,
        createWriteStream(temporary, { flags: "wx", mode: 0o600 }),
      );
      if (hash.digest("hex") !== manifest.payloadSha256) throw new Error("backup checksum mismatch");
      await run("pg_restore", ["--list", temporary], { stdout: new Writable({ write(_chunk, _encoding, callback) { callback(); } }) });
      await rename(temporary, dump);
      await writeFile(marker, `${JSON.stringify({ ...manifest, stagedAt: new Date().toISOString() })}\n`, { flag: "wx", mode: 0o600 });
      process.stdout.write(`${JSON.stringify({ ...manifest, stagedAt: new Date().toISOString() })}\n`);
    } catch (error) {
      await rm(temporary, { force: true });
      await rm(dump, { force: true });
      throw error;
    }
  });
}

const command = process.argv[2];
if (command === "create") {
  await createBackup(option("scope"), option("output"));
} else if (command === "stage") {
  await stageRestore(option("scope"), option("input"), option("backup-dir"));
} else {
  throw new Error("usage: postgres-backup.mjs create|stage --scope <scope> ...");
}
