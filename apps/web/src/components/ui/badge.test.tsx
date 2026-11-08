import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Badge, runStatusVariant, serviceStatusVariant } from "./badge";

describe("Badge", () => {
  it("renders children with the requested variant classes", () => {
    render(<Badge variant="danger">ошибка</Badge>);
    expect(screen.getByText("ошибка").className).toContain("text-danger");
  });

  it("defaults to the neutral variant", () => {
    render(<Badge>нейтрально</Badge>);
    expect(screen.getByText("нейтрально").className).toContain("text-muted-foreground");
  });

  it("maps run and service statuses to variants", () => {
    expect(runStatusVariant("queued")).toBe("neutral");
    expect(runStatusVariant("running")).toBe("info");
    expect(runStatusVariant("cancel_requested")).toBe("warning");
    expect(runStatusVariant("done")).toBe("success");
    expect(runStatusVariant("failed")).toBe("danger");
    expect(runStatusVariant("unknown")).toBe("neutral");
    expect(serviceStatusVariant("ready")).toBe("success");
    expect(serviceStatusVariant("not_ready")).toBe("warning");
    expect(serviceStatusVariant("unreachable")).toBe("danger");
  });
});
