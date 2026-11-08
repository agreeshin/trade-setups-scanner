import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AuthGate } from "./auth-gate";
import { SessionSlot } from "./ui/app-frame";
import { apiFetch, fetchPlatform, SESSION_EXPIRED_EVENT } from "../lib/platform-api";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
const session = () => new Response(JSON.stringify({ authenticated: true, expiresAt: Date.now() + 60_000 }));

describe("browser authentication", () => {
  it("gates the app, handles invalid key, logs in without persisting key and logs out", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(new Response(null, { status: 401 }))
      .mockResolvedValueOnce(new Response(null, { status: 401 }))
      .mockImplementationOnce(() => Promise.resolve(session()))
      .mockResolvedValueOnce(new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetch);
    render(<AuthGate><><p>Protected content</p><SessionSlot /></></AuthGate>);
    expect(screen.queryByText("Protected content")).not.toBeInTheDocument();
    const input = await screen.findByLabelText("API-ключ");
    fireEvent.change(input, { target: { value: "wrong" } });
    fireEvent.click(screen.getByRole("button", { name: "Войти" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Неверный API-ключ");
    expect(input).toHaveValue("");
    fireEvent.change(input, { target: { value: "correct" } });
    fireEvent.click(screen.getByRole("button", { name: "Войти" }));
    expect(await screen.findByText("Protected content")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Выйти" }));
    expect(await screen.findByLabelText("API-ключ")).toBeInTheDocument();
    expect(screen.queryByText("Protected content")).not.toBeInTheDocument();
    for (const [, init] of fetch.mock.calls as [string, RequestInit][]) {
      expect(init.credentials).toBe("same-origin");
      expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
    }
  });

  it("removes protected UI when API reports an expired session", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementationOnce(() => Promise.resolve(session())).mockResolvedValueOnce(new Response(null, { status: 401 })));
    render(<AuthGate><p>Protected content</p></AuthGate>);
    await screen.findByText("Protected content");
    await apiFetch("/v1/setups");
    expect(await screen.findByRole("alert")).toHaveTextContent("Сессия истекла");
    expect(screen.queryByText("Protected content")).not.toBeInTheDocument();
  });

  it("reports offline service and preserves fetchPlatform contract", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("offline")));
    render(<AuthGate>Protected</AuthGate>);
    expect(await screen.findByRole("alert")).toHaveTextContent("Сервис недоступен");
    expect(await fetchPlatform("/v1/setups")).toEqual({ kind: "unreachable", detail: "offline" });
  });

  it("keeps caller headers, enforces credentials and signals unauthorized requests", async () => {
    const fetch = vi.fn().mockResolvedValue(new Response(null, { status: 401 }));
    vi.stubGlobal("fetch", fetch);
    const expired = vi.fn();
    window.addEventListener(SESSION_EXPIRED_EVENT, expired);
    try {
      await apiFetch("/v1/settings", { method: "PUT", credentials: "omit", headers: { "Content-Type": "application/json" } });
      await waitFor(() => expect(expired).toHaveBeenCalledOnce());
      const [url, init] = fetch.mock.calls[0] as [string, RequestInit];
      expect(url).toBe("/v1/settings");
      expect(init.credentials).toBe("same-origin");
      expect(new Headers(init.headers).get("Content-Type")).toBe("application/json");
    } finally { window.removeEventListener(SESSION_EXPIRED_EVENT, expired); }
  });
});
