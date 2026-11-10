import { createContext, useContext } from "react";

export const RouteActiveContext = createContext(true);

/** True when the hosting page is the active navigation tab; defaults to true outside keep-alive shells. */
export function useRouteActive(): boolean {
  return useContext(RouteActiveContext);
}
