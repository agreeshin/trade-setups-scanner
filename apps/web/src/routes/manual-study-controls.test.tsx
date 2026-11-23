import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import type { OptimizationStudyAction } from "../lib/optimization-api";
import { ManualStudyActions, StudyStatusBadge } from "./optimization-results";
import type { OptimizationController } from "./use-optimization-studies";

function controller(patch: Partial<OptimizationController> = {}): OptimizationController {
  return { byAlgorithm: {}, latestByAlgorithm: {}, active: null, ready: true, error: null, creating: false, cancelling: null, deleting: null, controlling: null, revision: 0,
    start: vi.fn(() => Promise.resolve(true)), stop: vi.fn(() => Promise.resolve(true)), pause: vi.fn(() => Promise.resolve(true)), play: vi.fn(() => Promise.resolve(true)), remove: vi.fn(() => Promise.resolve(true)), clearError: vi.fn(), refresh: vi.fn(), ...patch };
}
const study = (status: OptimizationStudyAction["status"]): OptimizationStudyAction => ({ id: 7, origin: "manual", status, manualControl: { revision: 3, supportsPause: true } });
afterEach(() => { cleanup(); vi.restoreAllMocks(); });

it.each([
  ["queued", "Ожидает очередь"], ["pausing", "Постановка на паузу"], ["paused", "На паузе"],
] as const)("renders the exact %s server badge", (status, label) => {
  render(<StudyStatusBadge study={study(status)} />);
  expect(screen.getByText(label)).toBeInTheDocument();
});

it("does not infer Pause/Play support for legacy or continuous records", () => {
  const commands = controller();
  const view = render(<ManualStudyActions study={{ id: 7, status: "running" }} controller={commands} compact />);
  expect(screen.getByRole("button", { name: "Приостановить исследование 7" })).toBeDisabled();
  view.rerender(<ManualStudyActions study={{ id: 7, status: "paused", manualControl: null }} controller={commands} compact />);
  expect(screen.getByRole("button", { name: "Возобновить исследование 7" })).toBeDisabled();
  view.rerender(<ManualStudyActions study={{ ...study("running"), origin: "continuous" }} controller={commands} compact />);
  expect(screen.queryByRole("button")).not.toBeInTheDocument();
  expect(commands.pause).not.toHaveBeenCalled(); expect(commands.play).not.toHaveBeenCalled(); expect(commands.stop).not.toHaveBeenCalled();
});

it("keeps request feedback distinct from confirmed pausing/paused and never offers Play before paused", () => {
  const commands = controller();
  const view = render(<ManualStudyActions study={study("running")} controller={commands} compact />);
  fireEvent.click(screen.getByRole("button", { name: "Приостановить исследование 7" }));
  expect(commands.pause).toHaveBeenCalledExactlyOnceWith(study("running"));
  view.rerender(<ManualStudyActions study={study("running")} controller={controller({ controlling: { id: 7, action: "pause" } })} compact />);
  expect(screen.getByText("Запрашиваем паузу…")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Приостановить исследование 7" })).toBeDisabled();
  expect(screen.queryByText("На паузе")).not.toBeInTheDocument();
  view.rerender(<ManualStudyActions study={study("pausing")} controller={commands} compact />);
  expect(screen.getByText("Постановка на паузу")).toBeInTheDocument();
  expect(screen.getByRole("status", { name: "Ожидание команды исследования" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Остановить исследование 7" })).toBeEnabled();
  expect(screen.queryByRole("button", { name: "Возобновить исследование 7" })).not.toBeInTheDocument();
  view.rerender(<ManualStudyActions study={study("paused")} controller={commands} compact />);
  fireEvent.click(screen.getByRole("button", { name: "Возобновить исследование 7" }));
  expect(commands.play).toHaveBeenCalledExactlyOnceWith(study("paused"));
  view.rerender(<ManualStudyActions study={study("queued")} controller={commands} compact />);
  expect(screen.queryByRole("button")).not.toBeInTheDocument();
});

it("requires irreversible Stop confirmation, focuses Cancel, supports Escape, restores focus and deduplicates confirmation", async () => {
  let finish: (ok: boolean) => void = () => undefined;
  const stop = vi.fn(() => new Promise<boolean>((resolve) => { finish = resolve; }));
  const commands = controller({ stop });
  render(<ManualStudyActions study={study("paused")} controller={commands} compact />);
  const trigger = screen.getByRole("button", { name: "Остановить исследование 7" });
  fireEvent.click(trigger);
  const confirmation = within(screen.getByRole("dialog", { name: "Остановить исследование #7?" }));
  expect(confirmation.getByText(/Остановка необратима/)).toHaveTextContent(/частичный результат сохранится/);
  await waitFor(() => expect(confirmation.getByRole("button", { name: "Отмена" })).toHaveFocus());
  await userEvent.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  expect(trigger).toHaveFocus(); expect(stop).not.toHaveBeenCalled();
  fireEvent.click(trigger);
  const confirm = within(screen.getByRole("dialog")).getByRole("button", { name: "Остановить исследование" });
  fireEvent.click(confirm); fireEvent.click(confirm);
  expect(stop).toHaveBeenCalledExactlyOnceWith(study("paused"));
  expect(screen.getByRole("button", { name: "Запрашиваем остановку…" })).toBeDisabled();
  await act(async () => { finish(true); await Promise.resolve(); });
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
});

it("rechecks Stop eligibility when the server becomes terminal while confirmation is open", () => {
  const commands = controller();
  const view = render(<ManualStudyActions study={study("running")} controller={commands} compact />);
  fireEvent.click(screen.getByRole("button", { name: "Остановить исследование 7" }));
  view.rerender(<ManualStudyActions study={study("cancelled")} controller={commands} compact />);
  const confirm = within(screen.getByRole("dialog")).getByRole("button", { name: "Остановить исследование" });
  expect(confirm).toBeDisabled(); fireEvent.click(confirm);
  expect(commands.stop).not.toHaveBeenCalled();
  expect(screen.queryByRole("button", { name: "Возобновить исследование 7" })).not.toBeInTheDocument();
});

it("never retargets an open irreversible Stop confirmation when latest discovery changes to another study ID", () => {
  const commands = controller();
  const view = render(<ManualStudyActions study={study("running")} controller={commands} compact />);
  fireEvent.click(screen.getByRole("button", { name: "Остановить исследование 7" }));
  view.rerender(<ManualStudyActions study={{ ...study("running"), id: 8 }} controller={commands} compact />);
  const confirmation = within(screen.getByRole("dialog", { name: "Остановить исследование #7?" }));
  const confirm = confirmation.getByRole("button", { name: "Остановить исследование" });
  expect(confirm).toBeDisabled(); fireEvent.click(confirm);
  expect(commands.stop).not.toHaveBeenCalled();
});
