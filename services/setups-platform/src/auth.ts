import type { FastifyReply, FastifyRequest } from "fastify";

export const API_KEY_HEADER = "x-api-key";

export function createApiKeyGuard(validKeys: string[]) {
  const validKeySet = new Set(validKeys.filter((key) => key.length > 0));
  return async function requireApiKey(
    request: FastifyRequest,
    reply: FastifyReply,
  ): Promise<void> {
    const provided = request.headers[API_KEY_HEADER];
    const key = Array.isArray(provided) ? provided[0] : provided;
    if (key === undefined || !validKeySet.has(key)) {
      await reply.code(401).send({ error: "unauthorized" });
    }
  };
}
