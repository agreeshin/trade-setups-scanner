import eslint from "@eslint/js";
import tseslint from "typescript-eslint";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";
import { webGuardConfig } from "./eslint.guard.mjs";

const reactHooksFlat =
  reactHooks.configs.flat?.["recommended-latest"] ??
  reactHooks.configs["recommended-latest"];

export default tseslint.config(
  {
    ignores: [
      "**/dist/**",
      "**/node_modules/**",
      "**/coverage/**",
      "**/playwright-report/**",
      "**/test-results/**",
      "services/setups-platform-rs/**",
    ],
  },
  eslint.configs.recommended,
  ...tseslint.configs.recommendedTypeChecked,
  ...tseslint.configs.stylisticTypeChecked,
  {
    languageOptions: {
      parserOptions: {
        projectService: true,
      },
    },
  },
  {
    files: ["apps/web/**/*.{ts,tsx}"],
    ...reactHooksFlat,
  },
  {
    files: ["**/*.{js,mjs}"],
    ...tseslint.configs.disableTypeChecked,
    languageOptions: {
      ...tseslint.configs.disableTypeChecked.languageOptions,
      globals: globals.node,
    },
  },
  {
    // The guard test imports root-level .mjs modules. Their types resolve via
    // sibling .d.mts declarations in tsc, but the ESLint TypeScript project
    // service reports them as unresolved, so unsafe-* rules are disabled for
    // this single file. The test itself verifies guard rule correctness.
    files: ["apps/web/test/design-system-guard.test.ts"],
    rules: {
      "@typescript-eslint/no-unsafe-argument": "off",
      "@typescript-eslint/no-unsafe-assignment": "off",
      "@typescript-eslint/no-unsafe-call": "off",
      "@typescript-eslint/no-unsafe-member-access": "off",
    },
  },
  ...webGuardConfig,
);
