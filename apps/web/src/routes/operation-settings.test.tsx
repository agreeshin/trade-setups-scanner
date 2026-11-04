import { afterEach, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { OperationSettings } from "./operation-settings";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

it("reads and saves replay pause through the platform without automatic mutation", async () => {
  let paused = false;
  const writes: unknown[] = [];
  vi.stubGlobal("fetch", vi.fn((_url: string, init?: RequestInit) => {
    if (init?.method === "PUT") {
      if (typeof init.body !== "string") throw new Error("expected JSON request body");
      const body = JSON.parse(init.body) as { paused: boolean };
      writes.push(body); paused = body.paused;
    }
    return Promise.resolve(new Response(JSON.stringify({ paused }), { status: 200 }));
  }));
  render(<OperationSettings />);
  await screen.findByText("Очередь исторических расчётов работает");
  expect(writes).toHaveLength(0);
  await userEvent.click(screen.getByRole("button", { name: "Приостановить расчёты" }));
  await screen.findByText("Исторические расчёты приостановлены");
  expect(writes).toEqual([{ paused: true }]);
  await userEvent.click(screen.getByRole("button", { name: "Продолжить расчёты" }));
  await screen.findByText("Очередь исторических расчётов работает");
  expect(writes).toEqual([{ paused: true }, { paused: false }]);
});
