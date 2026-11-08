import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "./collapsible";

describe("Collapsible", () => {
  it("reveals content after trigger click", () => {
    render(
      <Collapsible>
        <CollapsibleTrigger>Подробности</CollapsibleTrigger>
        <CollapsibleContent>Метрики сетапа</CollapsibleContent>
      </Collapsible>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Подробности" }));
    expect(screen.getByText("Метрики сетапа")).toBeVisible();
  });
});
