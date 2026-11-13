/* global self */
self.addEventListener("push", (event) => {
  let data;
  try { data = event.data?.json(); } catch { return; }
  const now = Date.now();
  if (!data || typeof data.symbol !== "string" || !Number.isFinite(data.timestamp) ||
    !Number.isFinite(data.expiresAt) || data.expiresAt < now || data.timestamp > now) return;
  const symbol = data.symbol.slice(0, 80);
  const direction = data.recommendation?.direction;
  const adaptive = direction === "short" || direction === "long";
  const message = typeof data.message === "string" ? data.message : typeof data.body === "string" ? data.body : null;
  const tag = Number.isSafeInteger(data.setupId) && data.setupId > 0 ? `setup-${data.setupId}`
    : `watch-${symbol}-${data.timestamp}${Number.isSafeInteger(data.slotId) ? `-slot-${data.slotId}` : ""}`;
  event.waitUntil(self.registration.showNotification(`${adaptive ? direction.toUpperCase() : "WATCH"} ${symbol}`, {
    body: message ? message.slice(0, 4000) : adaptive ? "Сохранена адаптивная рекомендация. Условия и время возможного входа — в ленте сетапов. Условие не подтверждено."
      : `${symbol} · ${new Date(data.timestamp).toISOString()}`,
    timestamp: data.timestamp,
    tag,
  }));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  // Never accept a navigation destination from the push payload.
  event.waitUntil(self.clients.openWindow(new URL("/setups", self.location.origin).href));
});
