import type { SpikeSetup } from "./detector/price-volume-spike.js";

export interface DeliveryTarget {
  /** Seconds elapsed from the signal minute end; null when undeliverable. */
  ageSeconds: number | null;
  eligible: boolean;
}

export function evaluateDeliveryAge(
  setup: Pick<SpikeSetup, "closeTimeUnix">,
  nowUnix: number,
  maxAgeSeconds: number,
): DeliveryTarget {
  const age = nowUnix - setup.closeTimeUnix;
  if (age < 0) {
    return { ageSeconds: null, eligible: false };
  }
  return { ageSeconds: age, eligible: age <= maxAgeSeconds };
}

export interface Notifier {
  readonly kind: string;
  send(setup: SpikeSetup): Promise<void>;
}

/** MVP notifier: logs eligible setups; real channels arrive with deployment. */
export class LogNotifier implements Notifier {
  readonly kind = "log";

  send(setup: SpikeSetup): Promise<void> {
    console.log(
      `[setup] ${setup.algorithmId}@${setup.algorithmVersion} close=${setup.closePrice} ` +
        `openTimeUnix=${setup.openTimeUnix} closeTimeUnix=${setup.closeTimeUnix}`,
    );
    return Promise.resolve();
  }
}
