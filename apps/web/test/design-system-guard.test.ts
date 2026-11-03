import { describe, expect, it } from "vitest";
import { Linter } from "eslint";
import { webGuardConfig } from "../../../eslint.guard.mjs";
import { findIllegalStyleImports } from "../../../scripts/check-styles.mjs";
const linter = new Linter({ configType: "flat" });
function lintWebCode(code: string): string[] {
  const messages = linter.verify(code, webGuardConfig, {
    filename: "apps/web/src/fixtures/sample.tsx",
  });
  return messages.map((message) => message.ruleId ?? message.message);
}
describe("design system guard rules", () => {
  it("rejects inline style attributes in JSX", () => {
    const ruleIds = lintWebCode(
      `export function Badge() {\n  return <span style={{ color: "red" }}>hot</span>;\n}\n`,
    );
    expect(ruleIds).toContain("no-restricted-syntax");
  });
  it("rejects third-party UI library imports", () => {
    const ruleIds = lintWebCode(
      `import { DatePicker } from "antd";\nexport const x = 1;\n`,
    );
    expect(ruleIds).toContain("no-restricted-imports");
  });
  it("rejects CSS-in-JS imports", () => {
    const ruleIds = lintWebCode(
      `import styled from "styled-components";\nexport const Box = 1;\n`,
    );
    expect(ruleIds).toContain("no-restricted-imports");
  });
  it("allows semantic markup with Tailwind classes and the local UI layer", () => {
    const ruleIds = lintWebCode(
      `import { Button } from "../components/ui/button";\nexport function Toolbar() {\n  return (\n    <section className="p-4">\n      <Button variant="outline">Фильтр</Button>\n    </section>\n  );\n}\n`,
    );
    expect(ruleIds).toEqual([]);
  });
  it("allows importing only the single system stylesheet", () => {
    const violations = findIllegalStyleImports([
      { path: "apps/web/src/main.tsx", content: `import "./styles/global.css";\n` },
    ]);
    expect(violations).toEqual([]);
  });
  it("rejects style imports outside the system stylesheet", () => {
    const violations = findIllegalStyleImports([
      { path: "apps/web/src/routes/page.tsx", content: `import "./page.module.css";\n` },
      { path: "apps/web/src/other.tsx", content: `import "../styles/local.css";\n` },
    ]);
    expect(violations).toHaveLength(2);
    expect(violations[0]?.specifier).toBe("./page.module.css");
    expect(violations[1]?.specifier).toBe("../styles/local.css");
  });
});
