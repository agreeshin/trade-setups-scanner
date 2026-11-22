import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { SetupOutcomeBadge, StudyStatusBadge } from "./entity-status-badge";
afterEach(cleanup);
it.each([["tp", "TP"], ["sl", "SL"], ["unclosed", "Не закрылась"], ["no_data", "Нет данных"], ["gate_failed", "Гейт не пройден"]])("renders setup %s as a textual library Badge", (status, label) => {
  render(<SetupOutcomeBadge status={status} />);
  expect(screen.getByText(label)).toHaveAttribute("data-slot", "badge");
});
it.each([["queued", "В очереди"], ["running", "Выполняется"], ["cancel_requested", "Отмена запрошена"], ["completed", "Завершено"], ["cancelled", "Отменено"], ["failed", "Ошибка"]])("renders study %s as a textual library Badge", (status, label) => {
  render(<StudyStatusBadge status={status} />);
  expect(screen.getByText(label)).toHaveAttribute("data-slot", "badge");
});
it("keeps absent recommendation/outcome and unknown states explicit and non-success", () => {
  render(<><SetupOutcomeBadge recommended={false} /><SetupOutcomeBadge /><SetupOutcomeBadge status="future_setup" /><StudyStatusBadge status="future_study" /></>);
  expect(screen.getByText("Неприменимо")).toHaveAttribute("data-slot", "badge");
  expect(screen.getByText("Не рассчитано")).toHaveAttribute("data-slot", "badge");
  expect(screen.getAllByText("Неизвестный статус")).toHaveLength(2);
  for (const badge of document.querySelectorAll('[data-slot="badge"]')) expect(badge.className).not.toContain("text-success");
});
