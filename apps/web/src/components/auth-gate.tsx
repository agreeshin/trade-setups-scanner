import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { apiFetch, SESSION_EXPIRED_EVENT } from "../lib/platform-api";
import { Button } from "./ui/button";
import { Input } from "./ui/input";
import { Card } from "./ui/card";
import { AuthFrame, SessionBar } from "./ui/app-frame";

export function AuthGate({ children }: { children: ReactNode }) {
  const [state, setState] = useState<"checking" | "anonymous" | "authenticated">("checking");
  const [apiKey, setApiKey] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [expiresAt, setExpiresAt] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    const expired = () => {
      setState("anonymous");
      setError("Сессия истекла. Войдите снова.");
    };
    window.addEventListener(SESSION_EXPIRED_EVENT, expired);
    void apiFetch("/v1/session", { signal: controller.signal }).then(async (response) => {
      if (controller.signal.aborted) return;
      if (response.ok) {
        const body = await response.json() as { expiresAt: number };
        if (controller.signal.aborted) return;
        setExpiresAt(body.expiresAt);
        setState("authenticated");
      } else {
        setState("anonymous");
        if (response.status !== 401) setError("Не удалось проверить сессию. Попробуйте войти снова.");
      }
    }).catch(() => {
      if (!controller.signal.aborted) {
        setState("anonymous");
        setError("Сервис недоступен. Проверьте соединение и попробуйте снова.");
      }
    });
    return () => { controller.abort(); window.removeEventListener(SESSION_EXPIRED_EVENT, expired); };
  }, []);
  useEffect(() => {
    if (state !== "authenticated") return;
    const timer = window.setTimeout(() => {
      setState("anonymous");
      setError("Сессия истекла. Войдите снова.");
    }, Math.max(0, expiresAt - Date.now()));
    return () => window.clearTimeout(timer);
  }, [expiresAt, state]);
  async function login(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setError("");
    const key = apiKey;
    setApiKey("");
    try {
      const response = await apiFetch("/v1/session", {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ apiKey: key }),
      });
      if (!response.ok) {
        setError(response.status === 401 ? "Неверный API-ключ." : response.status === 429
          ? "Слишком много попыток. Подождите минуту." : "Не удалось войти. Проверьте адрес сервиса и попробуйте снова.");
        return;
      }
      const body = await response.json() as { expiresAt: number };
      setExpiresAt(body.expiresAt);
      setState("authenticated");
    } catch { setError("Сервис недоступен. Проверьте соединение и попробуйте снова."); }
    finally { setBusy(false); }
  }
  async function logout() {
    setBusy(true);
    setError("");
    try {
      const response = await apiFetch("/v1/session", { method: "DELETE" });
      if (!response.ok) throw new Error("logout failed");
      setState("anonymous");
    } catch { setError("Не удалось выйти. Попробуйте снова."); }
    finally { setBusy(false); }
  }
  if (state === "checking") return <p role="status" className="p-6 text-muted-foreground">Проверка сессии...</p>;
  if (state === "authenticated") return <>
    <SessionBar>
      {error && <p role="alert">{error}</p>}
      <Button variant="ghost" disabled={busy} onClick={() => { void logout(); }}>Выйти</Button>
    </SessionBar>
    {children}
  </>;
  return <AuthFrame>
    <Card asChild><form onSubmit={(event) => { void login(event); }} className="w-full max-w-sm space-y-4">
      <h1 className="text-xl font-semibold">Вход в платформу</h1>
      <p className="text-sm text-muted-foreground">Используйте API-ключ администратора. Ключ не сохраняется в браузере.</p>
      {window.location.protocol !== "https:" && <p className="text-sm text-muted-foreground">HTTP не шифрует ключ и сессию. Используйте только доверенную сеть; для защиты соединения нужен HTTPS.</p>}
      <label className="block space-y-2"><span>API-ключ</span>
        <Input type="password" autoComplete="off" required value={apiKey} onChange={(event) => setApiKey(event.target.value)} />
      </label>
      {error && <p role="alert" className="text-sm">{error}</p>}
      <Button type="submit" variant="success" disabled={busy}>{busy ? "Вход..." : "Войти"}</Button>
    </form></Card>
  </AuthFrame>;
}
