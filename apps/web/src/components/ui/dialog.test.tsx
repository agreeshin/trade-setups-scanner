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
    expect(document.querySelector('[data-slot="dialog-content"]')).not.toHaveClass("overflow-hidden", "max-h-[85dvh]", "fixed");
    const overlay = document.querySelector('[data-slot="dialog-overlay"]');
    expect(overlay?.className).toContain("backdrop-blur-[1.5px]");
    expect(overlay).toHaveClass("overflow-y-auto", "[scrollbar-width:none]");
    expect(overlay).toContainElement(screen.getByRole("dialog"));
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

  it("stacks complete overlay/content layers and keeps the parent scroll lock until both close", async () => {
    const user = userEvent.setup();
    render(<Dialog><DialogTrigger asChild><button>История</button></DialogTrigger><DialogContent>
      <DialogTitle>Все оптимизации</DialogTitle><DialogDescription>Список</DialogDescription>
      <Dialog><DialogTrigger asChild><button>Результат #17</button></DialogTrigger><DialogContent>
        <DialogTitle>Исследование #17</DialogTitle><DialogDescription>Сохранённый результат</DialogDescription>
        <p>Произвольно длинное содержимое</p>
      </DialogContent></Dialog>
    </DialogContent></Dialog>);
    await user.click(screen.getByRole("button", { name: "История" }));
    const parent = screen.getByRole("dialog", { name: "Все оптимизации" });
    await user.click(screen.getByRole("button", { name: "Результат #17" }));
    const top = screen.getByRole("dialog", { name: "Исследование #17" });
    const layers = document.querySelectorAll('[data-slot="dialog-overlay"]');
    expect(layers).toHaveLength(2);
    expect(layers[0]).toContainElement(parent);
    expect(layers[1]).toContainElement(top);
    expect(layers[0]).toHaveClass("z-50"); expect(layers[1]).toHaveClass("z-50", "backdrop-blur-[1.5px]");
    await user.click(screen.getByRole("button", { name: "Закрыть" }));
    expect(screen.getByRole("dialog", { name: "Все оптимизации" })).toBe(parent);
    expect(document.body.style.pointerEvents).toBe("none");
    expect(document.body).toHaveAttribute("data-scroll-locked");
    expect(screen.getByRole("button", { name: "Результат #17" })).toHaveFocus();
    await user.click(screen.getByRole("button", { name: "Закрыть" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(document.body.style.pointerEvents).toBe("");
    expect(document.body).not.toHaveAttribute("data-scroll-locked");
    expect(screen.getByRole("button", { name: "История" })).toHaveFocus();
  });
});
