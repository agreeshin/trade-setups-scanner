import { Link, useOutletContext } from "react-router";
import { Button } from "../components/ui/button";
import { marketDataLabel, type StatusContext } from "./app-layout";

export function OverviewPage() {
  const { status, refresh } = useOutletContext<StatusContext>();
  return <section aria-labelledby="overview-heading">
    <h1 id="overview-heading" className="text-2xl font-semibold">Обзор</h1>
    <div className="mt-4 grid max-w-2xl gap-3 rounded-md border border-input p-4">
      <h2 className="text-lg font-semibold">Состояние сервисов</h2>
      {status.kind === "loading" && <p role="status">Проверка доступности…</p>}
      {status.kind === "unreachable" && <p role="alert">Платформа недоступна ({status.detail}). Статус сервиса данных не получен.</p>}
      {status.kind === "ready" && <dl className="grid grid-cols-2 gap-2"><dt>Платформа сигналов</dt><dd>готова</dd><dt>Сервис данных</dt><dd>{marketDataLabel(status.data.marketData)}</dd></dl>}
      <p className="text-sm text-muted-foreground">Последний ответ API доступности. Это не проверка полноты истории или работы live-сканера.</p>
      <Button variant="outline" disabled={status.kind === "loading"} onClick={refresh}>Обновить состояние</Button>
    </div>
    <nav aria-label="Работа с платформой" className="mt-6 flex flex-wrap gap-4 text-sm">
      <Link className="underline" to="/setups">Лента live-сетапов</Link><Link className="underline" to="/runs">Исторические запуски</Link><Link className="underline" to="/settings">Алгоритмы и live-сканирование</Link>
      <Link className="underline" to="/history">История и сбор свечей</Link>
    </nav>
  </section>;
}
