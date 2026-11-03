import { FormEvent, useEffect, useState } from "react";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { PLATFORM_URL, fetchPlatform } from "../lib/platform-api";

export function SettingsPage() {
  const [maxAge, setMaxAge] = useState("");
  const [status, setStatus] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ maxAgeSeconds: number }>("/v1/settings/notifications").then(
      (state) => {
        if (cancelled) {
          return;
        }
        if (state.kind === "ready") {
          setMaxAge(String(state.data.maxAgeSeconds));
        } else if (state.kind === "unreachable") {
          setStatus(`Платформа недоступна (${state.detail})`);
        }
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  const handleSave = (event: FormEvent<HTMLFormElement>): void => {
    event.preventDefault();
    const seconds = Number(maxAge);
    if (!Number.isFinite(seconds) || seconds <= 0) {
      setStatus("Возраст должен быть положительным числом секунд.");
      return;
    }
    setSaving(true);
    void fetch(`${PLATFORM_URL}/v1/settings/notifications`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxAgeSeconds: seconds }),
    })
      .then((response) => {
        setStatus(response.ok ? "Сохранено." : `Платформа ответила ${response.status}`);
      })
      .catch((error: unknown) => {
        setStatus(error instanceof Error ? error.message : "network error");
      })
      .finally(() => {
        setSaving(false);
      });
  };

  return (
    <section aria-labelledby="settings-heading">
      <h1 id="settings-heading" className="text-2xl font-semibold">
        Настройки
      </h1>
      <form className="mt-4 grid max-w-xl gap-3" onSubmit={handleSave}>
        <label className="grid gap-1 text-sm">
          Максимальный возраст уведомления (секунд)
          <Input
            value={maxAge}
            onChange={(event) => setMaxAge(event.target.value)}
            placeholder="60"
          />
        </label>
        <p className="text-xs text-muted-foreground">
          Просроченные сетапы остаются в ленте, но не рассылаются как свежие.
        </p>
        {status !== null && (
          <p role="status" className="text-sm text-muted-foreground">
            {status}
          </p>
        )}
        <Button type="submit" disabled={saving}>
          {saving ? "Сохранение…" : "Сохранить"}
        </Button>
      </form>
    </section>
  );
}
