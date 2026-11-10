import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { useLocation } from "react-router";
import { AnalyticsPage } from "./analytics-page";
import { HistoryPage } from "./history-page";
import { OverviewPage } from "./overview-page";
import { RunsPage } from "./runs-page";
import { RouteActiveContext } from "./route-active";
import { SettingsPage } from "./settings-page";
import { SetupsPage } from "./setups-page";

/** Stable page registry: element identity must not change between renders, otherwise pages remount. */
const PAGES: readonly { path: string; element: ReactNode }[] = [
  { path: "/", element: <OverviewPage /> },
  { path: "/setups", element: <SetupsPage /> },
  { path: "/analytics", element: <AnalyticsPage /> },
  { path: "/runs", element: <RunsPage /> },
  { path: "/settings", element: <SettingsPage /> },
  { path: "/history", element: <HistoryPage /> },
];

/** Navigation tabs stay mounted after the first visit: inactive pages are hidden instead of unmounted,
 * like the segmented panels inside Analytics, so their state, results and running computations survive. */
export function KeepAlivePages() {
  const { pathname } = useLocation();
  const [visited, setVisited] = useState<ReadonlySet<string>>(() => new Set([pathname]));
  const scrollByPath = useRef(new Map<string, number>());
  const activePath = useRef(pathname);

  if (!visited.has(pathname)) {
    const next = new Set(visited);
    next.add(pathname);
    setVisited(next);
  }

  // Track the live scroll offset of the active page before it gets hidden, so it can be restored later.
  useEffect(() => {
    const onScroll = () => scrollByPath.current.set(activePath.current, window.scrollY);
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  // Restore the incoming page's saved offset once the outgoing page is hidden (display:none) again.
  useLayoutEffect(() => {
    if (activePath.current === pathname) return;
    activePath.current = pathname;
    window.scrollTo({ top: scrollByPath.current.get(pathname) ?? 0 });
  }, [pathname]);

  return <>
    {PAGES.map((page) => visited.has(page.path) ? (
      <RouteActiveContext.Provider key={page.path} value={page.path === pathname}>
        <div hidden={page.path !== pathname} data-page-path={page.path}>{page.element}</div>
      </RouteActiveContext.Provider>
    ) : null)}
  </>;
}
