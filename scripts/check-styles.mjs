#!/usr/bin/env node
import { readdir, readFile } from "node:fs/promises";
import { join, normalize, relative } from "node:path";
const ROOT = new URL("..", import.meta.url).pathname;
const SCAN_ROOT = join(ROOT, "apps/web");
const ALLOWED_STYLE_IMPORTS = new Set([normalize("apps/web/src/styles/global.css")]);
// This test contains literal import strings as rule fixtures, not real imports.
const FIXTURE_FILES = new Set(["apps/web/test/design-system-guard.test.ts"]);
const STYLE_IMPORT_RE =
  /(?:import|export)\s+(?:[^"';]*?\sfrom\s+)?["']([^"']+\.(?:css|scss|sass|less|styl))(?:\?[^"']*)?["']/g;
/**
 * @param {Array<{path: string, content: string}>} sources
 *   Files with paths relative to the repository root.
 * @returns {Array<{path: string, specifier: string, resolved: string}>}
 */
export function findIllegalStyleImports(sources) {
  const violations = [];
  for (const source of sources) {
    for (const match of source.content.matchAll(STYLE_IMPORT_RE)) {
      const specifier = match[1] ?? "";
      const resolved = specifier.startsWith(".")
        ? normalize(relative(ROOT, join(ROOT, source.path, "..", specifier)))
        : specifier;
      if (!ALLOWED_STYLE_IMPORTS.has(resolved)) {
        violations.push({ path: source.path, specifier, resolved });
      }
    }
  }
  return violations;
}
async function collectSourceFiles(dir, accumulator) {
  const entries = await readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    if (entry.name === "node_modules" || entry.name === "dist") {
      continue;
    }
    const fullPath = join(dir, entry.name);
    if (entry.isDirectory()) {
      await collectSourceFiles(fullPath, accumulator);
    } else if (/\.(?:ts|tsx|mjs|js)$/.test(entry.name)) {
      accumulator.push(fullPath);
    }
  }
  return accumulator;
}
if (process.argv[1]?.endsWith("check-styles.mjs")) {
  const allFiles = await collectSourceFiles(SCAN_ROOT, []);
  const files = allFiles.filter(
    (file) => !FIXTURE_FILES.has(relative(ROOT, file)),
  );
  const sources = await Promise.all(
    files.map(async (file) => ({
      path: relative(ROOT, file),
      content: await readFile(file, "utf8"),
    })),
  );
  const violations = findIllegalStyleImports(sources);
  if (violations.length > 0) {
    console.error("Illegal style imports found:");
    for (const violation of violations) {
      console.error(
        `  ${violation.path}: '${violation.specifier}' (resolved: ${violation.resolved})`,
      );
    }
    console.error(
      `Only ${[...ALLOWED_STYLE_IMPORTS].join(", ")} may be imported as styles.`,
    );
    process.exitCode = 1;
  } else {
    console.log("Style import check passed.");
  }
}
