/**
 * Exact decimal arithmetic on BigInt. Trading thresholds (`>= 12%`, strict
 * `> 3x`) must never depend on binary floating point rounding, so detector
 * comparisons operate on scaled integers only.
 */
export interface Decimal {
  int: bigint;
  scale: number;
}

export function parseDecimal(value: string): Decimal {
  const trimmed = value.trim();
  const point = trimmed.indexOf(".");
  if (point === -1) {
    return { int: BigInt(trimmed), scale: 0 };
  }
  const intPart = trimmed.slice(0, point);
  const fracPart = trimmed.slice(point + 1);
  return { int: BigInt(intPart + fracPart), scale: fracPart.length };
}

export const ZERO_DECIMAL: Decimal = { int: 0n, scale: 0 };

function align(a: Decimal, b: Decimal): [bigint, bigint] {
  if (a.scale === b.scale) {
    return [a.int, b.int];
  }
  if (a.scale < b.scale) {
    return [a.int * 10n ** BigInt(b.scale - a.scale), b.int];
  }
  return [a.int, b.int * 10n ** BigInt(a.scale - b.scale)];
}

export function addDecimals(a: Decimal, b: Decimal): Decimal {
  if (a.scale === b.scale) {
    return { int: a.int + b.int, scale: a.scale };
  }
  const [ai, bi] = align(a, b);
  return { int: ai + bi, scale: Math.max(a.scale, b.scale) };
}

export function subDecimals(a: Decimal, b: Decimal): Decimal {
  if (a.scale === b.scale) {
    return { int: a.int - b.int, scale: a.scale };
  }
  const [ai, bi] = align(a, b);
  return { int: ai - bi, scale: Math.max(a.scale, b.scale) };
}

export function mulByBigInt(a: Decimal, factor: bigint): Decimal {
  return { int: a.int * factor, scale: a.scale };
}

/**
 * Compares `a * aMul` versus `b * bMul` where multipliers are positive
 * integers. Returns -1, 0 or 1. Keeps full precision by scaling to a common
 * decimal scale before multiplying.
 */
export function compareScaled(
  a: Decimal,
  aMul: bigint,
  b: Decimal,
  bMul: bigint,
): number {
  const [ai, bi] = align(a, b);
  const left = ai * aMul;
  const right = bi * bMul;
  if (left < right) {
    return -1;
  }
  if (left > right) {
    return 1;
  }
  return 0;
}
