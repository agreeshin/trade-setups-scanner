import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { RunOriginBadge } from "./runs-page";

afterEach(cleanup);
it("uses current run_json/batch_json actual provenance fields for the special badge", () => {
  render(<RunOriginBadge origin="study_actual" actualStudyId={49} actualRevision={3} />);
  expect(screen.getByText("После исследования")).toHaveClass("text-ambient-violet");
  expect(screen.getByText("Исследование #49 · ревизия 3")).toBeInTheDocument();
});
it("does not classify manual or continuous runs by their names or old continuous identifiers", () => {
  render(<><RunOriginBadge origin="manual" actualStudyId={49} actualRevision={3} /><RunOriginBadge origin="continuous" /></>);
  expect(screen.queryByText("После исследования")).not.toBeInTheDocument();
});
