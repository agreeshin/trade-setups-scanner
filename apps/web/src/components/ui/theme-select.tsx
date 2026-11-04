import { useEffect, useState } from "react";
import { Select } from "./select";

function themeValue(value: string | null | undefined) {
  return value === "light" || value === "dark" ? value : "system";
}

export function ThemeSelect() {
  const [theme, setTheme] = useState(() => themeValue(document.documentElement.dataset.theme));
  useEffect(() => {
    const sync = (event: StorageEvent) => {
      if (event.key !== "trade-setups-theme" && event.key !== null) return;
      const next = themeValue(event.newValue);
      document.documentElement.dataset.theme = next;
      setTheme(next);
    };
    window.addEventListener("storage", sync);
    return () => { window.removeEventListener("storage", sync); };
  }, []);
  return <Select aria-label="Тема интерфейса" className="h-8 w-auto text-xs" value={theme} onChange={(event) => {
    const next = themeValue(event.target.value);
    document.documentElement.dataset.theme = next;
    setTheme(next);
    try { localStorage.setItem("trade-setups-theme", next); }
    catch { /* Keep the selection for this page when browser storage is denied. */ }
  }}>
    <option value="light">Светлая</option>
    <option value="dark">Тёмная</option>
    <option value="system">Системная</option>
  </Select>;
}
