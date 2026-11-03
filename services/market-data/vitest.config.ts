import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    setupFiles: ["./test/setup.ts"],
    include: ["test/**/*.test.ts"],
    // Integration files share one physical test database and clean tables in
    // beforeAll, so test files must not run in parallel.
    fileParallelism: false,
  },
});
