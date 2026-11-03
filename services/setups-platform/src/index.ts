import { buildApp } from "./app.js";
const port = Number(process.env.PORT ?? 8082);
const host = process.env.HOST ?? "127.0.0.1";
const app = await buildApp();
app
  .listen({ port, host })
  .then((address) => {
    app.log.info(`signals platform listening at ${address}`);
  })
  .catch((error) => {
    app.log.error(error, "failed to start signals platform");
    process.exit(1);
  });
