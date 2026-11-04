import { afterEach, describe, expect, it, vi } from "vitest";
import { buildApp } from "../src/app.js";
import { createSessionAuth } from "../src/auth.js";
import Fastify from "fastify";

const headers = { "x-platform-request": "1", origin: "http://localhost:80" };
afterEach(() => vi.restoreAllMocks());

describe("browser sessions", () => {
  it("bounds session retention and invalidates sessions across process instances", async () => {
    const app = Fastify();
    const auth = createSessionAuth(["secret"]);
    auth.register(app);
    const otherApp = Fastify();
    createSessionAuth(["secret"]).register(otherApp);
    try {
      let oldestCookie = "";
      let newestCookie = "";
      for (let index = 0; index < 129; index++) {
        const result = await app.inject({ method: "POST", url: "/v1/session", headers, payload: { apiKey: "secret" } });
        expect(result.statusCode).toBe(200);
        newestCookie = String(result.headers["set-cookie"]);
        if (index === 0) oldestCookie = newestCookie;
      }
      expect((await app.inject({ url: "/v1/session", headers: { cookie: oldestCookie } })).statusCode).toBe(401);
      expect((await app.inject({ url: "/v1/session", headers: { cookie: newestCookie } })).statusCode).toBe(200);
      expect((await otherApp.inject({ url: "/v1/session", headers: { cookie: newestCookie } })).statusCode).toBe(401);
      await app.close();
      const reopened = Fastify();
      try {
        // Closing an app destroys all sessions owned by that instance.
        auth.register(reopened);
        expect((await reopened.inject({ url: "/v1/session", headers: { cookie: newestCookie } })).statusCode).toBe(401);
      } finally { await reopened.close(); }
    } finally { await app.close(); await otherApp.close(); }
  });

  it("issues an opaque signed cookie, checks session, expires and logs out", async () => {
    const app = await buildApp({ apiKeys: ["admin-secret"], openApiEnabled: false });
    try {
      expect((await app.inject("/v1/session")).statusCode).toBe(401);
      const now = Date.now();
      vi.spyOn(Date, "now").mockReturnValue(now);
      const login = await app.inject({ method: "POST", url: "/v1/session", headers, payload: { apiKey: "admin-secret" } });
      expect(login.statusCode).toBe(200);
      const cookie = String(login.headers["set-cookie"]);
      expect(cookie).toContain("HttpOnly; SameSite=Strict; Max-Age=28800");
      expect(cookie).not.toContain("admin-secret");
      expect(cookie).not.toContain("Secure");
      const session = await app.inject({ url: "/v1/session", headers: { cookie } });
      expect(session.statusCode).toBe(200);
      expect(session.headers["cache-control"]).toBe("no-store");
      expect((await app.inject({ url: "/v1/session", headers: { cookie: cookie.replace("platform_session=", "platform_session=0") } })).statusCode).toBe(401);
      vi.spyOn(Date, "now").mockReturnValue(now + 8 * 60 * 60 * 1000);
      expect((await app.inject({ url: "/v1/session", headers: { cookie } })).statusCode).toBe(401);
      vi.spyOn(Date, "now").mockReturnValue(now);
      const logout = await app.inject({ method: "DELETE", url: "/v1/session", headers: { ...headers, cookie } });
      expect(logout.statusCode).toBe(204);
      expect(logout.headers["set-cookie"]).toContain("Max-Age=0");
      expect((await app.inject({ url: "/v1/session", headers: { cookie } })).statusCode).toBe(401);
    } finally { await app.close(); }
  });

  it("requires browser header and matching origin; marks HTTPS cookies Secure", async () => {
    const app = await buildApp({ apiKeys: ["secret"], openApiEnabled: false });
    try {
      for (const badHeaders of [{}, { ...headers, origin: "http://evil.test" }, { ...headers, "sec-fetch-site": "cross-site" }]) {
        expect((await app.inject({ method: "POST", url: "/v1/session", headers: badHeaders, payload: { apiKey: "secret" } })).statusCode).toBe(403);
        expect((await app.inject({ method: "DELETE", url: "/v1/session", headers: badHeaders })).statusCode).toBe(403);
      }
      const response = await app.inject({ method: "POST", url: "/v1/session", headers: { ...headers, host: "platform.test", origin: "https://platform.test" }, payload: { apiKey: "secret" } });
      expect(response.statusCode).toBe(200);
      expect(response.headers["set-cookie"]).toContain("; Secure");
    } finally { await app.close(); }
  });

  it("rate limits invalid logins without retaining per-client state", async () => {
    const app = await buildApp({ apiKeys: ["secret"], openApiEnabled: false });
    const now = Date.now();
    vi.spyOn(Date, "now").mockReturnValue(now);
    try {
      for (let attempt = 0; attempt < 10; attempt++) {
        expect((await app.inject({ method: "POST", url: "/v1/session", headers, payload: { apiKey: "wrong" } })).statusCode).toBe(401);
      }
      const limited = await app.inject({ method: "POST", url: "/v1/session", headers, payload: { apiKey: "secret" } });
      expect(limited.statusCode).toBe(429);
      expect(limited.headers["retry-after"]).toBe("60");
      vi.spyOn(Date, "now").mockReturnValue(now + 60_001);
      expect((await app.inject({ method: "POST", url: "/v1/session", headers, payload: { apiKey: "secret" } })).statusCode).toBe(200);
    } finally { await app.close(); }
  });

  it("preserves API-key clients but checks CSRF on cookie-auth mutations", async () => {
    const app = Fastify();
    const auth = createSessionAuth(["secret"]);
    auth.register(app);
    app.post("/protected", { preHandler: auth.requireApiKey }, () => ({ ok: true }));
    app.get("/protected", { preHandler: auth.requireApiKey }, () => ({ ok: true }));
    try {
      expect((await app.inject({ method: "POST", url: "/protected", headers: { "x-api-key": "secret" } })).statusCode).toBe(200);
      expect((await app.inject({ method: "POST", url: "/protected", headers: { "x-api-key": "wrong" } })).statusCode).toBe(401);
      const login = await app.inject({ method: "POST", url: "/v1/session", headers, payload: { apiKey: "secret" } });
      const cookie = String(login.headers["set-cookie"]);
      expect((await app.inject({ url: "/protected", headers: { cookie } })).statusCode).toBe(200);
      expect((await app.inject({ method: "POST", url: "/protected", headers: { cookie } })).statusCode).toBe(403);
      expect((await app.inject({ method: "POST", url: "/protected", headers: { ...headers, cookie } })).statusCode).toBe(200);
      expect((await app.inject({ method: "POST", url: "/protected", headers: { ...headers, origin: "http://evil.test", cookie } })).statusCode).toBe(403);
    } finally { await app.close(); }
  });
});
