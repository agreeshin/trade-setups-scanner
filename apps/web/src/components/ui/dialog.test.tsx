import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { Dialog, DialogContent, DialogDescription, DialogTitle, DialogTrigger } from "./dialog";

afterEach(cleanup);

describe("Dialog", () => {
  it("opens on trigger with a blurred overlay and closes on Escape", async () => {
    render(<Dialog>
      <DialogTrigger asChild><button>Показать список</button></DialogTrigger>
      <DialogContent>
        <DialogTitle>Проверенные наборы</DialogTitle>
        <DialogDescription>Снимок проверки</DialogDescription>
        <p>тело списка</p>
      </DialogContent>
    </Dialog>);
    expect(screen.queryByRole("dialog", { name: "Проверенные наборы" })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Показать список" }));
    expect(screen.getByRole("dialog", { name: "Проверенные наборы" })).toBeVisible();
    expect(screen.getByText("тело списка")).toBeVisible();
    expect(document.querySelector('[data-slot="dialog-content"]')?.className).toContain("overflow-hidden");
    const overlay = document.querySelector('[data-slot="dialog-overlay"]');
    expect(overlay?.className).toContain("backdrop-blur-[1.5px]");
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "Проверенные наборы" })).not.toBeInTheDocument();
  });

  it("closes via the explicit close button", async () => {
    render(<Dialog>
      <DialogTrigger asChild><button>Показать список</button></DialogTrigger>
      <DialogContent><DialogTitle>Заголовок</DialogTitle><DialogDescription>Описание</DialogDescription></DialogContent>
    </Dialog>);
    await userEvent.click(screen.getByRole("button", { name: "Показать список" }));
    expect(screen.getByRole("dialog", { name: "Заголовок" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Закрыть" }));
    expect(screen.queryByRole("dialog", { name: "Заголовок" })).not.toBeInTheDocument();
  });
});
