import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { createReadStream, createWriteStream } from "node:fs";
import { mkdir, readFile, rm, stat } from "node:fs/promises";
import { join } from "node:path";
import { Readable, Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import { Type } from "@sinclair/typebox";
import type { FastifyInstance, FastifyReply, FastifyRequest } from "fastify";

type ApiGuard = (request: FastifyRequest, reply: FastifyReply) => Promise<void>;

export interface BackupManagerOptions {
  scope: "platform" | "market-data";
  databaseUrl: string;
  backupDir: string;
  maxUploadBytes: number;
  scriptPath?: string;
}

interface BackupManifest {
  format: string;
  scope: string;
  createdAt: string;
  schemaVersion: string;
  postgresClient: string;
  payloadBytes: number;
  payloadSha256: string;
  stagedAt?: string;
}

export class BackupManager {
  readonly scope: BackupManagerOptions["scope"];
  readonly maxUploadBytes: number;
  readonly #databaseUrl: string;
  readonly #backupDir: string;
  readonly #scriptPath: string;

  constructor(options: BackupManagerOptions) {
    this.scope = options.scope;
    this.maxUploadBytes = options.maxUploadBytes;
    this.#databaseUrl = options.databaseUrl;
    this.#backupDir = options.backupDir;
    this.#scriptPath = options.scriptPath
      ?? fileURLToPath(new URL("../../../../scripts/postgres-backup.mjs", import.meta.url));
  }

  async #run(args: string[]): Promise<BackupManifest> {
    const child = spawn(process.execPath, [this.#scriptPath, ...args], {
      env: { ...process.env, DATABASE_URL: this.#databaseUrl },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout = (stdout + chunk).slice(-16_000); });
    child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-4000); });
    const code = await new Promise<number | null>((resolve, reject) => {
      child.once("error", reject);
      child.once("close", resolve);
    });
    if (code !== 0) throw new Error(stderr.trim() || `backup helper failed (${code})`);
    return JSON.parse(stdout) as BackupManifest;
  }

  async create(): Promise<{ path: string; manifest: BackupManifest }> {
    await mkdir(this.#backupDir, { recursive: true, mode: 0o700 });
    const path = join(this.#backupDir, `${this.scope}-${randomUUID()}.tsb`);
    try {
      const manifest = await this.#run(["create", "--scope", this.scope, "--output", path]);
      return { path, manifest };
    } catch (error) {
      await rm(path, { force: true });
      throw error;
    }
  }

  async stage(payload: Readable): Promise<BackupManifest> {
    await mkdir(this.#backupDir, { recursive: true, mode: 0o700 });
    const upload = join(this.#backupDir, `${this.scope}-${randomUUID()}.upload`);
    let bytes = 0;
    const limiter = new Transform({
      transform: (chunk: Buffer, _encoding, callback) => {
        bytes += chunk.length;
        if (bytes > this.maxUploadBytes) callback(new Error(`backup exceeds ${this.maxUploadBytes} byte upload limit`));
        else callback(null, chunk);
      },
    });
    try {
      await pipeline(payload, limiter, createWriteStream(upload, { flags: "wx", mode: 0o600 }));
      return await this.#run(["stage", "--scope", this.scope, "--input", upload, "--backup-dir", this.#backupDir]);
    } finally {
      await rm(upload, { force: true });
    }
  }

  async status(): Promise<{ pending: boolean; last: unknown }> {
    const marker = join(this.#backupDir, `${this.scope}.restore.pending`);
    const last = join(this.#backupDir, `${this.scope}.restore.last.json`);
    const pending = await stat(marker).then(() => true, () => false);
    const lastStatus = await readFile(last, "utf8").then((value) => JSON.parse(value) as unknown, () => null);
    return { pending, last: lastStatus };
  }
}

const Status = Type.Object({ pending: Type.Boolean(), last: Type.Unknown() });

export function registerBackupRoutes(app: FastifyInstance, requireApiKey: ApiGuard, manager: BackupManager): void {
  app.addContentTypeParser("application/octet-stream", (request, payload, done) => done(null, payload));

  app.get("/v1/admin/backup", {
    preHandler: [requireApiKey],
  }, async (_request, reply) => {
    const backup = await manager.create();
    const size = await stat(backup.path);
    const cleanup = () => { void rm(backup.path, { force: true }); };
    reply.raw.once("close", cleanup);
    return reply
      .header("Cache-Control", "no-store")
      .header("Content-Type", "application/octet-stream")
      .header("Content-Length", size.size)
      .header("Content-Disposition", `attachment; filename="${manager.scope}-${backup.manifest.createdAt.replaceAll(":", "-")}.tsb"`)
      .send(createReadStream(backup.path));
  });

  app.post("/v1/admin/restore", {
    preHandler: [requireApiKey],
    bodyLimit: manager.maxUploadBytes,
  }, async (request, reply) => {
    if (request.headers["x-restore-confirmation"] !== `RESTORE ${manager.scope}`) {
      return reply.code(400).send({ error: `confirmation_required: RESTORE ${manager.scope}` });
    }
    if (!(request.body instanceof Readable)) return reply.code(400).send({ error: "binary_backup_required" });
    try {
      const manifest = await manager.stage(request.body);
      return reply.code(202).send({ status: "staged", scope: manager.scope, restartRequired: true, manifest });
    } catch (error) {
      const message = error instanceof Error ? error.message : "";
      if (/pending|already in progress/.test(message)) return reply.code(409).send({ error: "restore_already_pending" });
      if (message.includes("insufficient free space")) return reply.code(507).send({ error: "insufficient_restore_space" });
      if (/truncated|unsupported backup format|invalid backup manifest|scope mismatch|payload size mismatch|checksum mismatch|pg_restore:/.test(message)) {
        return reply.code(400).send({ error: "invalid_backup" });
      }
      throw error;
    }
  });

  app.get("/v1/admin/restore", {
    preHandler: [requireApiKey],
    schema: { response: { 200: Status } },
  }, async () => manager.status());
}

export function registerMarketBackupProxy(
  app: FastifyInstance,
  requireApiKey: ApiGuard,
  options: { url: string; apiKey: string; maxUploadBytes: number },
): void {
  app.get("/v1/admin/market-backup", { preHandler: [requireApiKey] }, async (_request, reply) => {
    const response = await fetch(`${options.url}/v1/admin/backup`, {
      headers: { "x-api-key": options.apiKey },
    });
    if (!response.ok || !response.body) {
      return reply.code(response.status).send({ error: `market_backup_failed:${response.status}` });
    }
    for (const name of ["content-type", "content-length", "content-disposition", "cache-control"]) {
      const value = response.headers.get(name);
      if (value) reply.header(name, value);
    }
    return reply.send(Readable.fromWeb(response.body));
  });

  app.post("/v1/admin/market-restore", {
    preHandler: [requireApiKey],
    bodyLimit: options.maxUploadBytes,
  }, async (request, reply) => {
    if (!(request.body instanceof Readable)) return reply.code(400).send({ error: "binary_backup_required" });
    const confirmation = request.headers["x-restore-confirmation"];
    const response = await fetch(`${options.url}/v1/admin/restore`, {
      method: "POST",
      headers: {
        "x-api-key": options.apiKey,
        "content-type": "application/octet-stream",
        ...(typeof confirmation === "string" ? { "x-restore-confirmation": confirmation } : {}),
      },
      body: request.body,
      duplex: "half",
    });
    const text = await response.text();
    return reply.code(response.status).type(response.headers.get("content-type") ?? "application/json").send(text);
  });

  app.get("/v1/admin/market-restore", { preHandler: [requireApiKey] }, async (_request, reply) => {
    const response = await fetch(`${options.url}/v1/admin/restore`, {
      headers: { "x-api-key": options.apiKey },
    });
    const text = await response.text();
    return reply.code(response.status).type(response.headers.get("content-type") ?? "application/json").send(text);
  });
}
