import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import type { FastifyInstance, FastifyReply, FastifyRequest } from "fastify";

export const API_KEY_HEADER = "x-api-key";
const COOKIE = "platform_session";
const SESSION_SECONDS = 24 * 60 * 60;

export function createSessionAuth(validKeys: string[]) {
  const keys = validKeys.filter(Boolean);
  // Process-owned signing key: restart (including API-key rotation) ends sessions.
  const signingKey = randomBytes(32);
  // Fixed-capacity active sessions allow logout revocation without growing history.
  const sessions = new Map<string, number>();
  let failures = 0;
  let windowEnd = 0;
  const validKey = (value: unknown): boolean =>
    typeof value === "string" && keys.some((key) => {
      const a = Buffer.from(value);
      const b = Buffer.from(key);
      return a.length === b.length && timingSafeEqual(a, b);
    });
  const sign = (value: string) => createHmac("sha256", signingKey).update(value).digest("hex");
  const sessionExpiry = (request: FastifyRequest): number | undefined => {
    const token = request.headers.cookie?.split(";").map((part) => part.trim())
      .find((part) => part.startsWith(`${COOKIE}=`))?.slice(COOKIE.length + 1);
    if (!token || token.length > 200) return;
    const [expiry, nonce, signature, extra] = token.split(".");
    if (!expiry || !nonce || !signature || extra !== undefined || !/^\d+$/.test(expiry)
      || !/^[a-f0-9]{32}$/.test(nonce) || !/^[a-f0-9]{64}$/.test(signature)) return;
    const expected = sign(`${expiry}.${nonce}`);
    if (!timingSafeEqual(Buffer.from(signature), Buffer.from(expected))) return;
    const expiresAt = Number(expiry);
    if (expiresAt <= Date.now() || expiresAt > Date.now() + SESSION_SECONDS * 1000 || sessions.get(nonce) !== expiresAt) return;
    return expiresAt;
  };
  const browserRequest = (request: FastifyRequest): boolean => {
    if (request.headers["x-platform-request"] !== "1") return false;
    const site = request.headers["sec-fetch-site"];
    if (site && site !== "same-origin" && site !== "none") return false;
    if (request.headers.origin !== undefined) {
      try {
        const origin = new URL(request.headers.origin);
        if (!["http:", "https:"].includes(origin.protocol) ||
          origin.origin !== new URL(`${origin.protocol}//${request.headers.host}`).origin) return false;
      } catch { return false; }
    }
    return true;
  };
  const cookie = (request: FastifyRequest, token: string, maxAge: number) => {
    // Origin is validated before issuance; this also handles HTTPS termination.
    const secure = request.protocol === "https" || request.headers.origin?.startsWith("https://");
    return `${COOKIE}=${token}; Path=/v1; HttpOnly; SameSite=Strict; Max-Age=${maxAge}${secure ? "; Secure" : ""}`;
  };
  const requireApiKey = async (request: FastifyRequest, reply: FastifyReply): Promise<void> => {
    if (validKey(request.headers[API_KEY_HEADER])) return;
    if (!sessionExpiry(request)) {
      await reply.code(401).send({ error: "unauthorized" });
      return;
    }
    if (!["GET", "HEAD", "OPTIONS"].includes(request.method) && !browserRequest(request)) {
      await reply.code(403).send({ error: "invalid_request_origin" });
    }
  };
  const register = (app: FastifyInstance) => {
    const cleanup = setInterval(() => {
      for (const [nonce, expiry] of sessions) if (expiry <= Date.now()) sessions.delete(nonce);
    }, 60_000);
    cleanup.unref();
    app.addHook("onClose", () => { clearInterval(cleanup); sessions.clear(); });
    app.post<{ Body: { apiKey?: unknown } }>("/v1/session", { bodyLimit: 4096 }, async (request, reply) => {
      reply.header("Cache-Control", "no-store");
      if (!browserRequest(request)) return reply.code(403).send({ error: "invalid_request_origin" });
      const now = Date.now();
      // Global fixed-window budget is constant memory and works behind a proxy.
      if (now >= windowEnd) { failures = 0; windowEnd = now + 60_000; }
      if (failures >= 10) return reply.header("Retry-After", Math.ceil((windowEnd - now) / 1000))
        .code(429).send({ error: "too_many_attempts" });
      if (!validKey(request.body?.apiKey)) {
        failures++;
        return reply.code(401).send({ error: "invalid_api_key" });
      }
      const expiresAt = now + SESSION_SECONDS * 1000;
      const nonce = randomBytes(16).toString("hex");
      if (sessions.size >= 128) {
        const oldest = sessions.keys().next().value;
        if (oldest !== undefined) sessions.delete(oldest);
      }
      sessions.set(nonce, expiresAt);
      const payload = `${expiresAt}.${nonce}`;
      return reply.header("Set-Cookie", cookie(request, `${payload}.${sign(payload)}`, SESSION_SECONDS))
        .send({ authenticated: true, expiresAt });
    });
    app.get("/v1/session", async (request, reply) => {
      reply.header("Cache-Control", "no-store");
      const expiresAt = sessionExpiry(request);
      if (!expiresAt) return reply.code(401).send({ error: "unauthorized" });
      return { authenticated: true, expiresAt };
    });
    app.delete("/v1/session", async (request, reply) => {
      reply.header("Cache-Control", "no-store");
      if (!browserRequest(request)) return reply.code(403).send({ error: "invalid_request_origin" });
      if (sessionExpiry(request)) {
        const token = request.headers.cookie?.split(";").map((part) => part.trim())
          .find((part) => part.startsWith(`${COOKIE}=`))?.slice(COOKIE.length + 1);
        const nonce = token?.split(".")[1];
        if (nonce) sessions.delete(nonce);
      }
      return reply.header("Set-Cookie", cookie(request, "", 0)).code(204).send();
    });
  };
  return { requireApiKey, register };
}
