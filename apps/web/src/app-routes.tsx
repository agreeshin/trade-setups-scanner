import { Route, Routes } from "react-router";
import { AppLayout } from "./routes/app-layout";
import { OverviewPage } from "./routes/overview-page";
import { SettingsPage } from "./routes/settings-page";
import { SetupsPage } from "./routes/setups-page";
import { RunsPage } from "./routes/runs-page";
import { HistoryPage } from "./routes/history-page";
import { AnalyticsPage } from "./routes/analytics-page";
export function AppRoutes() {
  return (
    <Routes>
      <Route element={<AppLayout />}>
        <Route index element={<OverviewPage />} />
        <Route path="setups" element={<SetupsPage />} />
        <Route path="analytics" element={<AnalyticsPage />} />
        <Route path="runs" element={<RunsPage />} />
        <Route path="history" element={<HistoryPage />} />
        <Route path="settings" element={<SettingsPage />} />
      </Route>
    </Routes>
  );
}
