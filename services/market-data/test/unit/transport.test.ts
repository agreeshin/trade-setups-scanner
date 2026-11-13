import { createServer, type Server, type RequestListener } from "node:http";
import { once } from "node:events";
import { afterEach, describe, expect, it } from "vitest";
import { FetchTransport } from "../../src/ingest/transport.js";

describe("bounded HTTP transport", () => {
  let server: Server | undefined;
  afterEach(async () => {
    if (!server) return;
    server.closeAllConnections();
    await new Promise<void>((resolve) => server!.close(() => resolve()));
  });
  async function listen(handler: RequestListener) {
    server = createServer(handler);
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing HTTP test address");
    return `http://127.0.0.1:${address.port}`;
  }
  it("times out while waiting for headers", async () => {
    const url = await listen(() => undefined);
    await expect(new FetchTransport(40).fetch(url)).rejects.toThrow("timeout");
  });
  it("times out while reading a stalled body", async () => {
    const url = await listen((_req, res) => { res.writeHead(200); res.write("partial"); });
    await expect(new FetchTransport(40).fetch(url)).rejects.toThrow();
  });
  it("rejects oversized bodies and allows a later bounded request", async () => {
    const url = await listen((_req, res) => { res.end("x".repeat(65)); });
    await expect(new FetchTransport(1000, 64).fetch(url)).rejects.toThrow("byte limit");
    expect((await new FetchTransport(1000, 128).fetch(url)).bodyText).toHaveLength(65);
  });
  it("cancels active requests and honours an already-aborted caller", async () => {
    const url = await listen(() => undefined);
    const transport = new FetchTransport(1000);
    const pending = transport.fetch(url).catch((error: unknown) => error);
    transport.abortActive();
    expect(await pending).toBeInstanceOf(Error);
    const controller = new AbortController(); controller.abort();
    await expect(transport.fetch(url, controller.signal)).rejects.toThrow();
  });
});
