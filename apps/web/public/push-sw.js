/* global self */
self.addEventListener("push", (event) => {
  let data;
  try { data = event.data?.json(); } catch { return; }
  if (!data || typeof data.symbol !== "string" || !Number.isFinite(data.timestamp) ||
    !Number.isFinite(data.expiresAt) || data.expiresAt < Date.now() || data.timestamp > Date.now()) return;
  const symbol = data.symbol.slice(0, 80);
  event.waitUntil(self.registration.showNotification(`WATCH ${symbol}`, {
    body: `${symbol} · ${new Date(data.timestamp).toISOString()}`,
    timestamp: data.timestamp,
    tag: `watch-${symbol}-${data.timestamp}`,
  }));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  // Never accept a navigation destination from the push payload.
  event.waitUntil(self.clients.openWindow(new URL("/setups", self.location.origin).href));
});
