import { Route, Routes } from "react-router";
import { AppLayout } from "./routes/app-layout";
import { KeepAlivePages } from "./routes/keep-alive-pages";
export function AppRoutes() {
  return (
    <Routes>
      <Route element={<AppLayout />}>
        <Route path="*" element={<KeepAlivePages />} />
      </Route>
    </Routes>
  );
}
