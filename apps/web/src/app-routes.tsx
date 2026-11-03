import { Route, Routes } from "react-router";
import { AppLayout } from "./routes/app-layout";
import { OverviewPage } from "./routes/overview-page";
import { SettingsPage } from "./routes/settings-page";
import { SetupsPage } from "./routes/setups-page";
import { RunsPage } from "./routes/runs-page";
export function AppRoutes() {
  return (
    <Routes>
      <Route element={<AppLayout />}>
        <Route index element={<OverviewPage />} />
        <Route path="setups" element={<SetupsPage />} />
        <Route path="runs" element={<RunsPage />} />
        <Route path="settings" element={<SettingsPage />} />
      </Route>
    </Routes>
  );
}
