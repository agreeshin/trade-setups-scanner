import { useEffect, useState } from "react";
import { apiFetch } from "../lib/platform-api";
import { Button } from "./ui/button";

function applicationKey(value: string): Uint8Array<ArrayBuffer> {
  const decoded = atob(value.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(decoded, (character) => character.charCodeAt(0));
}

export function PushSettings() {
  const supported = window.isSecureContext && "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;
  const [subscription, setSubscription] = useState<PushSubscription | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  useEffect(() => {
    if (!supported) return;
    let active = true;
    void navigator.serviceWorker.getRegistration("/").then(async (registration) => {
      const existing = await registration?.pushManager.getSubscription();
      if (active) setSubscription(existing ?? null);
    }).catch(() => { if (active) setError("Не удалось проверить подписку браузера."); });
    return () => { active = false; };
  }, [supported]);

  async function subscribe() {
    setBusy(true); setError(""); setMessage("");
    try {
      // Permission is requested only inside this explicit user action.
      const permission = await Notification.requestPermission();
      if (permission !== "granted") {
        setError("Уведомления не разрешены. Разрешите их в настройках браузера.");
        return;
      }
      const response = await apiFetch("/v1/push");
      if (!response.ok) throw new Error("Не удалось получить настройки Web Push.");
      const { publicKey } = await response.json() as { publicKey: string };
      await navigator.serviceWorker.register("/push-sw.js", { scope: "/" });
      const registration = await navigator.serviceWorker.ready;
      const existing = await registration.pushManager.getSubscription();
      const current = existing ?? await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: applicationKey(publicKey) });
      setSubscription(current);
      const saved = await apiFetch("/v1/push/subscriptions", {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(current.toJSON()),
      });
      if (!saved.ok) throw new Error(saved.status === 409
        ? "Достигнут лимит 100 подписок. Удалите ненужную подписку и повторите."
        : "Подписка браузера создана, но сервер её не сохранил. Повторите подключение.");
      setMessage("Подписка сохранена на сервере. Будут доставляться новые live-сетапы в пределах допустимого возраста; доставка зависит от браузера и push-провайдера.");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Не удалось подключить Web Push.");
    } finally { setBusy(false); }
  }

  async function unsubscribe() {
    if (!subscription) return;
    setBusy(true); setError(""); setMessage("");
    try {
      const response = await apiFetch("/v1/push/subscriptions", {
        method: "DELETE", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ endpoint: subscription.endpoint }),
      });
      if (!response.ok) throw new Error("Сервер не удалил подписку. Повторите отключение.");
      await subscription.unsubscribe();
      setSubscription(null);
      setMessage("Подписка удалена с сервера. Уведомления для этого браузера отключены.");
    } catch { setError("Не удалось полностью отключить подписку. Повторите отключение."); }
    finally { setBusy(false); }
  }

  return <section className="space-y-3 rounded-md border border-input bg-card p-4" aria-label="Web Push">
    <h2 className="text-lg font-semibold">Web Push</h2>
    <p className="text-sm text-muted-foreground">Возраст уведомления ограничивает доставку, но сам по себе не включает её. Подключите этот браузер отдельно.</p>
    {!supported ? <p role="status" className="text-sm">Web Push недоступен: нужны HTTPS, Service Worker и поддержка Push API. На HTTP-адресе сервера подключение заблокировано.</p> : <>
      <p className="text-sm text-muted-foreground">{subscription
        ? "В браузере есть подписка. Обновите подключение, чтобы подтвердить её сохранение на сервере."
        : "Этот браузер пока не подключён."}</p>
      <div className="flex flex-wrap gap-2">
        <Button disabled={busy} onClick={() => { void subscribe(); }}>{subscription ? "Обновить подключение" : "Подключить уведомления"}</Button>
        <Button variant="outline" disabled={busy || !subscription} onClick={() => { void unsubscribe(); }}>Отключить уведомления</Button>
      </div>
    </>}
    {message && <p role="status" className="text-sm">{message}</p>}
    {error && <p role="alert" className="text-sm">{error}</p>}
  </section>;
}
