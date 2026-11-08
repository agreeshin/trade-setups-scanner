import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "./tabs";

describe("Tabs", () => {
  it("switches visible content by trigger", async () => {
    const user = userEvent.setup();
    render(
      <Tabs defaultValue="load">
        <TabsList aria-label="Разделы истории">
          <TabsTrigger value="load">Загрузка истории</TabsTrigger>
          <TabsTrigger value="collection">Текущий сбор</TabsTrigger>
        </TabsList>
        <TabsContent value="load">Контент загрузки</TabsContent>
        <TabsContent value="collection">Контент сбора</TabsContent>
      </Tabs>,
    );
    expect(screen.getByRole("tab", { name: "Загрузка истории" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByText("Контент загрузки")).toBeVisible();
    await user.click(screen.getByRole("tab", { name: "Текущий сбор" }));
    expect(screen.getByRole("tab", { name: "Текущий сбор" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByText("Контент сбора")).toBeVisible();
  });
});
