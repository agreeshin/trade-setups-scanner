/** Actual elapsed time, not wall-clock timestamps; output is bounded to three units. */
export function formatElapsedDuration(seconds: number | null | undefined): string {
  if (seconds == null || !Number.isFinite(seconds)) return "—";
  // BigInt preserves full finite durations without exponent notation or a display cap.
  const totalMinutes = BigInt(Math.floor(Math.max(0, seconds))) / 60n;
  if (totalMinutes === 0n) return "Менее 1 мин";
  const days = totalMinutes / 1440n;
  const hours = totalMinutes / 60n % 24n;
  const minutes = totalMinutes % 60n;
  return [days ? `${days} дн` : "", hours ? `${hours} ч` : "", minutes ? `${minutes} мин` : ""].filter(Boolean).join(" ");
}
