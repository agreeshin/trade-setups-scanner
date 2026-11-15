import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it } from "vitest";
import { RefreshCw } from "lucide-react";
import { InfoNote } from "./info-note";
import { IconButton } from "./icon-button";

afterEach(cleanup);

it("reveals info on hover, links its accessible description and dismisses on Escape", async () => {
  const user = userEvent.setup();
  render(<InfoNote>Общая остановка сохраняет историю.</InfoNote>);
  expect(screen.queryByText("Общая остановка сохраняет историю.")).not.toBeInTheDocument();
  await user.hover(screen.getByRole("button", { name: "Пояснение" }));
  expect(screen.getByRole("tooltip")).toHaveTextContent("Общая остановка сохраняет историю.");
  expect(screen.getByRole("button")).toHaveAttribute("aria-describedby", screen.getByRole("tooltip").id);
  await user.keyboard("{Escape}");
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
});

it("supports keyboard focus, touch clicks and cleans up its portal on unmount", async () => {
  const user = userEvent.setup();
  const view = render(<InfoNote label="Справка">Подробности</InfoNote>);
  await user.tab();
  expect(screen.getByRole("tooltip")).toHaveTextContent("Подробности");
  await user.keyboard("{Escape}");
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  await user.click(screen.getByRole("button"));
  expect(screen.getByRole("tooltip")).toBeInTheDocument();
  view.unmount();
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
});

it("keeps an icon action named without visible text", async () => {
  render(<IconButton label="Обновить статус" icon={RefreshCw} />);
  const button = screen.getByRole("button", { name: "Обновить статус" });
  expect(button).toHaveClass("size-9");
  expect(button).toHaveTextContent("");
  await userEvent.hover(button);
  expect(screen.getByRole("tooltip")).toHaveTextContent("Обновить статус");
});
