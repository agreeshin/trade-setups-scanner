import { setTimeout as delay } from "node:timers/promises";

export interface Clock {
  nowMs(): number;
  sleep(ms: number, signal?: AbortSignal): Promise<void>;
}

export class RealClock implements Clock {
  nowMs(): number {
    return Date.now();
  }

  sleep(ms: number, signal?: AbortSignal): Promise<void> {
    return delay(ms, undefined, { signal });
  }
}

export class FakeClock implements Clock {
  private currentMs: number;

  constructor(startMs = 0) {
    this.currentMs = startMs;
  }

  nowMs(): number {
    return this.currentMs;
  }

  sleep(ms: number): Promise<void> {
    this.currentMs += ms;
    return Promise.resolve();
  }

  get now(): number {
    return this.currentMs;
  }
}
