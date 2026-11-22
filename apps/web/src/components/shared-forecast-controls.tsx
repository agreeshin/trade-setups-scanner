import { AnalyticsTrendControls } from "./analytics-trend-controls";
import { Button } from "./ui/button";
import { loadSharedAnalyticsSettings, saveSharedAnalyticsSettings, updateSharedAnalyticsSettings, useSharedAnalyticsSettings } from "../lib/analytics-settings-store";

export function SharedForecastControls() {
  const state = useSharedAnalyticsSettings();
  const settings = state.settings;
  return <section aria-label="Общий прогноз Analytics и Сетапов" className="grid min-w-0 gap-2">
    <fieldset disabled={state.status !== "ready"} className="min-w-0">
      <legend className="text-sm font-medium">Модель графика · общие настройки</legend>
      <AnalyticsTrendControls scope="shared" mode={settings?.trendMode ?? "mean"} bend={settings?.trendBendDays ?? "linear"}
        forecast={settings?.forecast} blendLambda={settings?.trendBlendLambda ?? "0.7"} disableMeanBend
        onModeChange={(mode) => updateSharedAnalyticsSettings({ trendMode: mode })}
        onBlendModeChange={() => updateSharedAnalyticsSettings({ trendMode: "blend" })}
        onBendChange={(bend) => updateSharedAnalyticsSettings({ trendBendDays: bend })}
        onBlendLambdaChange={(lambda) => updateSharedAnalyticsSettings({ trendBlendLambda: lambda })}
        onForecastChange={(forecast) => updateSharedAnalyticsSettings({ forecast, ...(forecast.method === "trend" ? { trendBendDays: forecast.bendDays } : {}) })} />
    </fieldset>
    {(state.status === "idle" || state.status === "loading") && <p role="status" className="text-xs text-muted-foreground">Загрузка общих настроек прогноза…</p>}
    {state.error !== null && <p role="alert" className="text-sm text-danger">Не удалось загрузить общие настройки: {state.error} <Button variant="outline" size="sm" onClick={loadSharedAnalyticsSettings}>Повторить загрузку настроек</Button></p>}
    {state.saveStatus === "saving" && <p role="status" className="text-xs text-muted-foreground">Сохранение общих настроек…</p>}
    {state.saveError !== null && <p role="alert" className="text-sm text-danger">Ошибка сохранения настроек: {state.saveError} <Button variant="outline" size="sm" onClick={saveSharedAnalyticsSettings}>Повторить сохранение настроек</Button></p>}
  </section>;
}
