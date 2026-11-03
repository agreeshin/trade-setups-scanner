export interface Clock {
  nowMs(): number;
  sleep(ms: number): Promise<void>;
}

export class RealClock implements Clock {
  nowMs(): number {
    return Date.now();
  }

  sleep(ms: number): Promise<void> {
    return new Promise((resolve) => {
      setTimeout(resolve, ms);
    });
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
