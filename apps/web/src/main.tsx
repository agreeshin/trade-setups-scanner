import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router";
import { Provider } from "react-redux";
import "./styles/global.css";
import { AppRoutes } from "./app-routes";
import { store } from "./store";
import { AuthGate } from "./components/auth-gate";
import { AppSurface } from "./components/ui/app-frame";
const rootElement = document.getElementById("root");
if (!rootElement) {
  throw new Error("root element not found");
}

createRoot(rootElement).render(
  <StrictMode>
    <Provider store={store}>
      <BrowserRouter>
        <AppSurface><AuthGate><AppRoutes /></AuthGate></AppSurface>
      </BrowserRouter>
    </Provider>
  </StrictMode>,
);
