import { Link, useOutletContext } from "react-router";
import { Alert } from "../components/ui/alert";
import { Badge, serviceStatusVariant } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "../components/ui/card";
import { PageHeader } from "../components/ui/page-header";
import { Spinner } from "../components/ui/spinner";
import { marketDataLabel, type StatusContext } from "./app-layout";

export function OverviewPage() {
  const { status, refresh } = useOutletContext<StatusContext>();
  return <section aria-labelledby="overview-heading" className="grid gap-6">
    <PageHeader id="overview-heading" title="Обзор" />
    <Card className="max-w-2xl">
      <CardHeader>
        <CardTitle>Состояние сервисов</CardTitle>
        <CardDescription>Последний ответ API доступности. Это не проверка полноты истории или работы live-сканера.</CardDescription>
      </CardHeader>
      <CardContent>
        {status.kind === "loading" && <Spinner label="Проверка доступности…" />}
        {status.kind === "unreachable" && (
          <Alert variant="destructive" role="alert">Платформа недоступна ({status.detail}). Статус сервиса данных не получен.</Alert>
        )}
        {status.kind === "ready" && (
          <dl className="grid gap-4 sm:grid-cols-2">
            <div className="grid gap-1.5">
              <dt className="text-xs uppercase tracking-wide text-muted-foreground">Платформа сигналов</dt>
              <dd><Badge variant="success">готова</Badge></dd>
            </div>
            <div className="grid gap-1.5">
              <dt className="text-xs uppercase tracking-wide text-muted-foreground">Сервис данных</dt>
              <dd><Badge variant={serviceStatusVariant(status.data.marketData)}>{marketDataLabel(status.data.marketData)}</Badge></dd>
            </div>
          </dl>
        )}
      </CardContent>
      <CardFooter>
        <Button variant="outline" disabled={status.kind === "loading"} onClick={refresh}>Обновить состояние</Button>
      </CardFooter>
    </Card>
    <nav aria-label="Работа с платформой" className="flex flex-wrap gap-3 text-sm">
      <Link className="inline-flex items-center rounded-lg border border-input/70 bg-muted/40 px-4 py-2.5 font-medium text-foreground transition-colors hover:border-primary/50 hover:bg-accent/40 hover:text-primary" to="/setups">Лента live-сетапов</Link>
      <Link className="inline-flex items-center rounded-lg border border-input/70 bg-muted/40 px-4 py-2.5 font-medium text-foreground transition-colors hover:border-primary/50 hover:bg-accent/40 hover:text-primary" to="/runs">Исторические запуски</Link>
      <Link className="inline-flex items-center rounded-lg border border-input/70 bg-muted/40 px-4 py-2.5 font-medium text-foreground transition-colors hover:border-primary/50 hover:bg-accent/40 hover:text-primary" to="/settings">Алгоритмы и live-сканирование</Link>
      <Link className="inline-flex items-center rounded-lg border border-input/70 bg-muted/40 px-4 py-2.5 font-medium text-foreground transition-colors hover:border-primary/50 hover:bg-accent/40 hover:text-primary" to="/history">История и сбор свечей</Link>
    </nav>
  </section>;
}
