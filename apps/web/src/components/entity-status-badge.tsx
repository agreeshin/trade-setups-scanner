import { Badge, type BadgeVariant } from "./ui/badge";

const outcomes: Record<string, { label: string; variant: BadgeVariant }> = {
  tp: { label: "TP", variant: "success" }, sl: { label: "SL", variant: "danger" },
  unclosed: { label: "Не закрылась", variant: "neutral" }, no_data: { label: "Нет данных", variant: "neutral" },
  gate_failed: { label: "Гейт не пройден", variant: "warning" },
};
const studies: Record<string, { label: string; variant: BadgeVariant }> = {
  queued: { label: "В очереди", variant: "neutral" }, running: { label: "Выполняется", variant: "info" },
  cancel_requested: { label: "Отмена запрошена", variant: "warning" }, completed: { label: "Завершено", variant: "success" },
  cancelled: { label: "Отменено", variant: "neutral" }, failed: { label: "Ошибка", variant: "danger" },
};

export function SetupOutcomeBadge({ status, recommended = true }: { status?: string | null | undefined; recommended?: boolean }) {
  const known = status == null ? null : outcomes[status];
  if (status == null) return <Badge>{recommended ? "Не рассчитано" : "Неприменимо"}</Badge>;
  return <Badge variant={known?.variant ?? "warning"} title={known ? undefined : status.slice(0, 120)}>{known?.label ?? "Неизвестный статус"}</Badge>;
}
export function StudyStatusBadge({ status }: { status?: string | null | undefined }) {
  const known = status == null ? null : studies[status];
  return <Badge variant={known?.variant ?? "warning"} title={known ? undefined : status?.slice(0, 120)}>{known?.label ?? "Неизвестный статус"}</Badge>;
}
