import { once } from "node:events";
import { WebSocketServer } from "ws";
import { afterEach, describe, expect, it, vi } from "vitest";
import { BybitClosedStream, DEFAULT_WS_OPTIONS, parseConfirmedBars, subscriptionShards } from "../../src/ingest/websocket.js";

const NOW = Date.UTC(2024, 5, 1, 0, 10);
const SYMBOL = "BTCUSDT";
function message(confirm: boolean) {
  return { topic: `kline.1.${SYMBOL}`, data: [{ confirm, interval: "1", start: NOW - 60_000, end: NOW - 1,
    open: "0.100000000000000000001", high: "2", low: "0.1", close: "1", volume: "0", turnover: "0" }] };
}

describe("Bybit closed-candle stream", () => {
  let server: WebSocketServer | undefined;
  let stream: BybitClosedStream | undefined;
  afterEach(async () => {
    stream?.stop();
    if (server) {
      for (const client of server.clients) client.terminate();
      await new Promise<void>((resolve) => server!.close(() => resolve()));
    }
  });
  async function listen() {
    server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
    await once(server, "listening");
    const address = server.address();
    if (typeof address === "string" || !address) throw new Error("missing WS test address");
    return `ws://127.0.0.1:${address.port}`;
  }
  it("shards 820 topics, deduplicates and respects the args character budget", () => {
    const shards = subscriptionShards(Array.from({ length: 820 }, (_, n) => `S${n}USDT`), 200);
    expect(shards.map((shard) => shard.length)).toEqual([200, 200, 200, 200, 20]);
    for (const shard of shards) expect(JSON.stringify(shard.map((s) => `kline.1.${s}`)).length).toBeLessThanOrEqual(21_000);
    expect(subscriptionShards([SYMBOL, SYMBOL], 200)).toEqual([[SYMBOL]]);
    expect(() => subscriptionShards(["bad symbol"], 200)).toThrow();
    expect(() => subscriptionShards([SYMBOL], 201)).toThrow();
  });
  it("preserves exact decimal strings, ignores unconfirmed/future/wrong-topic updates", () => {
    const allowed = new Set([SYMBOL]);
    expect(parseConfirmedBars(JSON.stringify(message(false)), allowed, NOW)).toEqual([]);
    expect(parseConfirmedBars(JSON.stringify(message(true)), allowed, NOW - 1)).toEqual([]);
    expect(parseConfirmedBars(JSON.stringify(message(true)), new Set(["ETHUSDT"]), NOW)).toEqual([]);
    const bars = parseConfirmedBars(JSON.stringify(message(true)), allowed, NOW);
    expect(bars[0]?.bar.open).toBe("0.100000000000000000001");
    const bad = message(true); bad.data[0]!.start += 1;
    expect(() => parseConfirmedBars(JSON.stringify(bad), allowed, NOW)).toThrow("interval");
  });
  it("waits for ACK, accepts only confirmed klines and cleans up on symbol removal", async () => {
    const url = await listen();
    const onBar = vi.fn(() => true);
    const events = vi.fn();
    server!.on("connection", (client) => {
      client.once("message", () => {
        client.send(JSON.stringify({ op: "subscribe", req_id: "closed-1m", success: true }));
        client.send(JSON.stringify(message(false)));
        client.send(JSON.stringify(message(true)));
      });
    });
    stream = new BybitClosedStream({ ...DEFAULT_WS_OPTIONS, url }, onBar, events, () => NOW);
    stream.setSymbols([SYMBOL]);
    await vi.waitFor(() => expect(onBar).toHaveBeenCalledTimes(1));
    expect(events).toHaveBeenCalledWith("ws_subscribed", "1 topics");
    stream.setSymbols([]);
    await vi.waitFor(() => expect(server!.clients.size).toBe(0));
  });
  it.each(["ack", "heartbeat", "payload"] as const)("reconnects after bounded %s failure and stop cancels reconnect", async (mode) => {
    const url = await listen();
    let connects = 0;
    server!.on("connection", (client) => {
      connects++;
      client.on("error", () => undefined);
      client.once("message", () => {
        if (mode !== "ack") client.send(JSON.stringify({ op: "subscribe", req_id: "closed-1m", success: true }));
        if (mode === "payload") client.send("x".repeat(513));
      });
    });
    stream = new BybitClosedStream({ ...DEFAULT_WS_OPTIONS, url, ackTimeoutMs: 40,
      heartbeatMs: 20, pongTimeoutMs: 15, reconnectMinMs: 10, reconnectMaxMs: 20, maxPayloadBytes: 512 }, () => true, () => undefined);
    stream.setSymbols([SYMBOL]);
    await vi.waitFor(() => expect(connects).toBeGreaterThanOrEqual(2));
    stream.stop();
    await vi.waitFor(() => expect(server!.clients.size).toBe(0));
    const stoppedAt = connects;
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(connects).toBe(stoppedAt);
  });
});
