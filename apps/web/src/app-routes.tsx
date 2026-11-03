import { Route, Routes } from "react-router";
import { AppLayout } from "./routes/app-layout";
import { OverviewPage } from "./routes/overview-page";
import { SettingsPage } from "./routes/settings-page";
export function AppRoutes() {
  return (
    <Routes>
      <Route element={<AppLayout />}>
        <Route index element={<OverviewPage />} />
        <Route path="settings" element={<SettingsPage />} />
      </Route>
    </Routes>
  );
}
