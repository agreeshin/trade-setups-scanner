const platformUrlFromEnv: unknown = import.meta.env.VITE_PLATFORM_URL;
export const PLATFORM_URL: string =
  typeof platformUrlFromEnv === "string" ? platformUrlFromEnv : "http://127.0.0.1:8082";

export type PlatformState<T> =
  | { kind: "loading" }
  | { kind: "unreachable"; detail: string }
  | { kind: "ready"; data: T };

export async function fetchPlatform<T>(path: string): Promise<PlatformState<T>> {
  try {
    const response = await fetch(`${PLATFORM_URL}${path}`);
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
