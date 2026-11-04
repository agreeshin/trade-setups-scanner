import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PushSettings } from "./push-settings";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function browser(permission = "granted") {
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("PushManager", vi.fn());
  const requestPermission = vi.fn().mockResolvedValue(permission);
  vi.stubGlobal("Notification", { requestPermission });
  const subscription = {
    endpoint: "https://fcm.googleapis.com/fake",
    toJSON: () => ({ endpoint: "https://fcm.googleapis.com/fake", keys: { p256dh: "fake", auth: "fake" } }),
    unsubscribe: vi.fn().mockResolvedValue(true),
  };
  const subscribe = vi.fn().mockResolvedValue(subscription);
  const registration = { pushManager: { getSubscription: vi.fn().mockResolvedValue(null), subscribe } };
  const serviceWorker = { getRegistration: vi.fn().mockResolvedValue(undefined), register: vi.fn().mockResolvedValue(registration), ready: Promise.resolve(registration) };
  vi.stubGlobal("navigator", { serviceWorker });
  const fetch = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
  fetch.mockResolvedValueOnce(new Response(JSON.stringify({ publicKey: "BAEBAQ" })));
  vi.stubGlobal("fetch", fetch);
  return { requestPermission, subscription, subscribe, serviceWorker, fetch };
}

describe("Web Push settings", () => {
  it("shows explicit HTTP blocker and does not offer unsupported buttons", () => {
    vi.stubGlobal("isSecureContext", false);
    render(<PushSettings />);
    expect(screen.getByRole("status")).toHaveTextContent("На HTTP-адресе сервера подключение заблокировано");
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  it("never prompts on mount, subscribes only on click, persists and deletes server subscription", async () => {
    const mocks = browser();
    render(<PushSettings />);
    await waitFor(() => expect(mocks.serviceWorker.getRegistration).toHaveBeenCalled());
    expect(mocks.requestPermission).not.toHaveBeenCalled();
    expect(mocks.serviceWorker.register).not.toHaveBeenCalled();
    expect(mocks.fetch).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Подключить уведомления" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Подписка сохранена на сервере");
    expect(mocks.requestPermission).toHaveBeenCalledOnce();
    expect(mocks.subscribe).toHaveBeenCalledOnce();
    expect(mocks.serviceWorker.register).toHaveBeenCalledWith("/push-sw.js", { scope: "/" });
    expect(mocks.fetch.mock.calls[1]?.[0]).toBe("/v1/push/subscriptions");
    const saved = mocks.fetch.mock.calls[1]?.[1] as RequestInit;
    expect(saved.method).toBe("POST");
    expect(saved.credentials).toBe("same-origin");
    expect(new Headers(saved.headers).get("x-platform-request")).toBe("1");
    fireEvent.click(screen.getByRole("button", { name: "Отключить уведомления" }));
    await waitFor(() => expect(screen.getByRole("status")).toHaveTextContent("Подписка удалена с сервера"));
    expect(mocks.subscription.unsubscribe).toHaveBeenCalledOnce();
    const deleted = mocks.fetch.mock.calls[2]?.[1] as RequestInit;
    expect(deleted.method).toBe("DELETE");
  });

  it("does not register or persist when permission is denied", async () => {
    const mocks = browser("denied");
    render(<PushSettings />);
    fireEvent.click(screen.getByRole("button", { name: "Подключить уведомления" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Уведомления не разрешены");
    expect(mocks.serviceWorker.register).not.toHaveBeenCalled();
    expect(mocks.fetch).not.toHaveBeenCalled();
  });

  it("does not claim delivery is enabled if server persistence fails", async () => {
    const mocks = browser();
    mocks.fetch.mockResolvedValueOnce(new Response(null, { status: 503 }));
    render(<PushSettings />);
    fireEvent.click(screen.getByRole("button", { name: "Подключить уведомления" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("сервер её не сохранил");
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Обновить подключение" })).toBeInTheDocument();
  });
});
