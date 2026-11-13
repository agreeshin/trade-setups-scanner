import WebSocket from "ws";
import { validateBar, type KlineBar } from "./bybit.js";

export interface WsOptions {
  url: string;
  shardSymbols: number;
  maxPayloadBytes: number;
  ackTimeoutMs: number;
  heartbeatMs: number;
  pongTimeoutMs: number;
  reconnectMinMs: number;
  reconnectMaxMs: number;
}
export const DEFAULT_WS_OPTIONS: WsOptions = {
  url: "wss://stream.bybit.com/v5/public/linear", shardSymbols: 200,
  maxPayloadBytes: 256 * 1024, ackTimeoutMs: 10_000,
  heartbeatMs: 20_000, pongTimeoutMs: 10_000,
  reconnectMinMs: 1000, reconnectMaxMs: 60_000,
};

export function subscriptionShards(symbols: string[], maxSymbols: number): string[][] {
  if (!Number.isInteger(maxSymbols) || maxSymbols < 1 || maxSymbols > 200) throw new Error("WS shard size must be 1..200");
  const shards: string[][] = [];
  let current: string[] = [];
  for (const symbol of [...new Set(symbols)].sort()) {
    if (!/^[A-Z0-9_]{1,64}$/.test(symbol)) throw new Error("invalid Bybit subscription symbol");
    const next = [...current, symbol];
    if (current.length && (next.length > maxSymbols || JSON.stringify(next.map((s) => `kline.1.${s}`)).length > 21_000)) {
      shards.push(current); current = [];
    }
    current.push(symbol);
  }
  if (current.length) shards.push(current);
  return shards;
}

export function parseConfirmedBars(text: string, symbols: ReadonlySet<string>, nowMs: number): { symbol: string; bar: KlineBar }[] {
  const payload = JSON.parse(text) as { topic?: string; data?: unknown[] };
  if (typeof payload.topic !== "string") return [];
  const symbol = payload.topic.startsWith("kline.1.") ? payload.topic.slice(8) : "";
  if (!symbols.has(symbol)) return [];
  if (!Array.isArray(payload.data) || payload.data.length > 10) throw new Error("invalid WS kline item count");
  const result: { symbol: string; bar: KlineBar }[] = [];
  for (const value of payload.data) {
    if (typeof value !== "object" || value === null) throw new Error("invalid WS kline");
    const row = value as Record<string, unknown>;
    if (row.confirm !== true) continue;
    if (row.interval !== "1" || typeof row.start !== "number" || typeof row.end !== "number" || row.end !== row.start + 59_999) throw new Error("invalid WS kline interval");
    const bar = { openTime: new Date(row.start), open: row.open, high: row.high, low: row.low,
      close: row.close, volume: row.volume, quoteTurnover: row.turnover } as KlineBar;
    validateBar(bar);
    if (row.start + 60_000 > nowMs) continue;
    result.push({ symbol, bar });
  }
  return result;
}

/** One bounded socket/timer owner per shard. No unbounded async message chains. */
export class BybitClosedStream {
  private shards: { symbols: string[]; stop: () => void }[] = [];
  private stopped = false;
  constructor(
    private readonly options: WsOptions,
    private readonly onBar: (symbol: string, bar: KlineBar) => boolean,
    private readonly onEvent: (event: string, detail: string) => void,
    private readonly now: () => number = Date.now,
  ) {}

  setSymbols(symbols: string[]): void {
    if (this.stopped) return;
    const wanted = subscriptionShards(symbols, this.options.shardSymbols);
    const previous = this.shards;
    this.shards = wanted.map((group) => {
      const key = group.join(",");
      const found = previous.find((shard) => shard.symbols.join(",") === key);
      return found ?? { symbols: group, stop: this.startShard(group) };
    });
    for (const shard of previous) if (!this.shards.includes(shard)) shard.stop();
  }

  stop(): void {
    this.stopped = true;
    for (const shard of this.shards) shard.stop();
    this.shards = [];
  }

  private startShard(symbols: string[]): () => void {
    let stopped = false;
    let socket: WebSocket | undefined;
    let reconnect: ReturnType<typeof setTimeout> | undefined;
    let ack: ReturnType<typeof setTimeout> | undefined;
    let heartbeat: ReturnType<typeof setInterval> | undefined;
    let pong: ReturnType<typeof setTimeout> | undefined;
    let attempt = 0;
    const allowed = new Set(symbols);
    const clear = () => { clearTimeout(ack); clearInterval(heartbeat); clearTimeout(pong); pong = undefined; };
    const disconnect = () => {
      clear();
      if (socket) {
        const old = socket; socket = undefined;
        old.removeAllListeners();
        // terminate() during handshake emits an expected asynchronous error.
        old.once("error", () => undefined);
        old.terminate();
      }
    };
    const retry = (reason: string) => {
      if (stopped || reconnect) return;
      disconnect();
      this.onEvent("ws_reconnecting", `${symbols.length} topics: ${reason.slice(0, 160)}`);
      const wait = Math.min(this.options.reconnectMaxMs, this.options.reconnectMinMs * 2 ** Math.min(attempt++, 16));
      reconnect = setTimeout(() => { reconnect = undefined; connect(); }, wait);
    };
    const connect = () => {
      if (stopped) return;
      const ws = new WebSocket(this.options.url, {
        maxPayload: this.options.maxPayloadBytes, perMessageDeflate: false,
        handshakeTimeout: this.options.ackTimeoutMs,
      });
      socket = ws;
      let acknowledged = false;
      ws.on("error", (error) => retry(error.message));
      ws.on("close", () => retry("socket closed"));
      ws.on("open", () => {
        ws.send(JSON.stringify({ op: "subscribe", req_id: "closed-1m", args: symbols.map((s) => `kline.1.${s}`) }));
        ack = setTimeout(() => retry("subscription ACK timeout"), this.options.ackTimeoutMs);
        heartbeat = setInterval(() => {
          if (pong || ws.readyState !== WebSocket.OPEN) return;
          ws.send(JSON.stringify({ op: "ping" }));
          pong = setTimeout(() => retry("heartbeat timeout"), this.options.pongTimeoutMs);
        }, this.options.heartbeatMs);
      });
      ws.on("message", (data, binary) => {
        try {
          const bytes = Array.isArray(data) ? Buffer.concat(data) : Buffer.isBuffer(data) ? data : Buffer.from(data);
          if (binary || bytes.byteLength > this.options.maxPayloadBytes) throw new Error("invalid WS message size/type");
          const text = bytes.toString("utf8");
          const message = JSON.parse(text) as { op?: string; req_id?: string; success?: boolean; ret_msg?: string };
          if (message.op === "subscribe") {
            if (message.req_id !== "closed-1m" || message.success !== true) throw new Error("subscription rejected");
            acknowledged = true; clearTimeout(ack);
            this.onEvent("ws_subscribed", `${symbols.length} topics`);
          } else if (message.op === "pong" || (message.op === "ping" && message.ret_msg === "pong")) {
            clearTimeout(pong); pong = undefined;
            if (acknowledged) attempt = 0;
          } else if (acknowledged) {
            for (const item of parseConfirmedBars(text, allowed, this.now())) {
              if (!this.onBar(item.symbol, item.bar)) throw new Error("fresh write queue full");
            }
          }
        } catch (error) { retry(error instanceof Error ? error.message : "invalid WS message"); }
      });
    };
    connect();
    return () => { stopped = true; clearTimeout(reconnect); disconnect(); };
  }
}
