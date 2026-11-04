const platformUrlFromEnv: unknown = import.meta.env.VITE_PLATFORM_URL;
export const PLATFORM_URL: string =
  typeof platformUrlFromEnv === "string" ? platformUrlFromEnv : "";

export const SESSION_EXPIRED_EVENT = "platform-session-expired";

export async function apiFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  headers.set("x-platform-request", "1");
  const response = await fetch(`${PLATFORM_URL}${path}`, {
    ...init, headers, credentials: "same-origin",
  });
  if (response.status === 401 && path !== "/v1/session") {
    window.dispatchEvent(new Event(SESSION_EXPIRED_EVENT));
  }
  return response;
}

export type PlatformState<T> =
  | { kind: "loading" }
  | { kind: "unreachable"; detail: string }
  | { kind: "ready"; data: T };

export async function fetchPlatform<T>(path: string, signal?: AbortSignal): Promise<PlatformState<T>> {
  try {
    const response = await apiFetch(path, signal ? { signal } : {});
    if (!response.ok) {
      return { kind: "unreachable", detail: `HTTP ${response.status}` };
    }
    return { kind: "ready", data: (await response.json()) as T };
  } catch (error) {
    return {
      kind: "unreachable",
      detail: error instanceof Error ? error.message : "network error",
    };
  }
}
