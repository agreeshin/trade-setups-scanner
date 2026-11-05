import { describe, expect, it, vi } from "vitest";
import { buildApp } from "../src/app.js";
import { BackupManager } from "../src/backup/routes.js";

const manifest = {
  format: "TRADE_SETUPS_BACKUP_V1",
  scope: "platform",
  createdAt: "2026-11-05T00:00:00.000Z",
  schemaVersion: "001",
  postgresClient: "pg_dump 17",
  payloadBytes: 4,
  payloadSha256: "hash",
};

describe("backup administration routes", () => {
  it("requires authentication and exact restore confirmation before streaming to staging", async () => {
    const manager = new BackupManager({
      scope: "platform",
      databaseUrl: "postgresql://unused/unused",
      backupDir: "/tmp/opencode/unused-backup-test",
      maxUploadBytes: 1024,
    });
    const stage = vi.spyOn(manager, "stage").mockResolvedValue(manifest);
    const app = await buildApp({ apiKeys: ["secret"], backupManager: manager });
    try {
      const unauthenticated = await app.inject({
        method: "POST", url: "/v1/admin/restore", headers: { "content-type": "application/octet-stream" }, payload: Buffer.from("dump"),
      });
      expect(unauthenticated.statusCode).toBe(401);

      const unconfirmed = await app.inject({
        method: "POST", url: "/v1/admin/restore", headers: { "content-type": "application/octet-stream", "x-api-key": "secret" }, payload: Buffer.from("dump"),
      });
      expect(unconfirmed.statusCode).toBe(400);
      expect(stage).not.toHaveBeenCalled();

      const accepted = await app.inject({
        method: "POST",
        url: "/v1/admin/restore",
        headers: { "content-type": "application/octet-stream", "x-api-key": "secret", "x-restore-confirmation": "RESTORE platform" },
        payload: Buffer.from("dump"),
      });
      expect(accepted.statusCode).toBe(202);
      expect(accepted.json()).toMatchObject({ status: "staged", scope: "platform", restartRequired: true });
      expect(stage).toHaveBeenCalledOnce();
    } finally {
      await app.close();
    }
  });

  it.each([
    ["backup checksum mismatch", 400, "invalid_backup"],
    ["a restore is already pending", 409, "restore_already_pending"],
    ["insufficient free space for restore", 507, "insufficient_restore_space"],
  ])("maps restore staging rejection %s to HTTP %i", async (message, status, responseError) => {
    const manager = new BackupManager({
      scope: "platform",
      databaseUrl: "postgresql://unused/unused",
      backupDir: "/tmp/opencode/unused-backup-test",
      maxUploadBytes: 1024,
    });
    vi.spyOn(manager, "stage").mockRejectedValue(new Error(message));
    const app = await buildApp({ apiKeys: ["secret"], backupManager: manager });
    try {
      const response = await app.inject({
        method: "POST",
        url: "/v1/admin/restore",
        headers: {
          "content-type": "application/octet-stream",
          "x-api-key": "secret",
          "x-restore-confirmation": "RESTORE platform",
        },
        payload: Buffer.from("dump"),
      });
      expect(response.statusCode).toBe(status);
      expect(response.json()).toEqual({ error: responseError });
      expect(response.body).not.toContain(message);
    } finally {
      await app.close();
    }
  });
});
