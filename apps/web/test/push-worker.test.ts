import { runInNewContext } from "node:vm";
import { describe, expect, it, vi } from "vitest";
import workerSource from "../public/push-sw.js?raw";

interface PushEventFixture {
  data?: { json: () => unknown };
  waitUntil: (promise: Promise<unknown>) => void;
  notification?: { close: () => void };
}
function worker() {
  const listeners = new Map<string, (event: PushEventFixture) => void>();
  const showNotification = vi.fn().mockResolvedValue(undefined);
  const openWindow = vi.fn().mockResolvedValue(undefined);
  runInNewContext(workerSource, {
    self: {
      addEventListener: (name: string, listener: (event: PushEventFixture) => void) => listeners.set(name, listener),
      registration: { showNotification }, clients: { openWindow }, location: { origin: "https://platform.test" },
    }, URL, Date,
  });
  return { listeners, showNotification, openWindow };
}
describe("push service worker", () => {
  it("displays neutral WATCH with symbol and timestamp, discards expired pushes", () => {
    const { listeners, showNotification } = worker();
    const timestamp = Date.now() - 1000;
    const waitUntil = vi.fn();
    listeners.get("push")?.({ data: { json: () => ({ symbol: "BTCUSDT", timestamp, expiresAt: Date.now() + 60_000 }) }, waitUntil });
    const [title, options] = showNotification.mock.calls[0] as [string, { timestamp: number; body: string }];
    expect(title).toBe("WATCH BTCUSDT");
    expect(options.timestamp).toBe(timestamp);
    expect(options.body).toContain("BTCUSDT");
    listeners.get("push")?.({ data: { json: () => ({ symbol: "BTCUSDT", timestamp, expiresAt: Date.now() - 1 }) }, waitUntil });
    expect(showNotification).toHaveBeenCalledOnce();
  });
  it("opens only same-origin setups, ignoring payload navigation", () => {
    const { listeners, openWindow } = worker();
    const close = vi.fn();
    listeners.get("notificationclick")?.({ notification: { close }, waitUntil: vi.fn(), data: { json: () => ({ url: "https://evil.test" }) } });
    expect(close).toHaveBeenCalledOnce();
    expect(openWindow).toHaveBeenCalledWith("https://platform.test/setups");
  });
});
