import { buildApp } from "./app.js";
const port = Number(process.env.PORT ?? 8081);
const host = process.env.HOST ?? "127.0.0.1";
const app = await buildApp();

app
  .listen({ port, host })
  .then((address) => {
    app.log.info(`market-data service listening at ${address}`);
  })
  .catch((error) => {
    app.log.error(error, "failed to start market-data service");
    process.exit(1);
  });
