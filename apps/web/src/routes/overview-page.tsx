import { useAppSelector } from "../store/hooks";

export function OverviewPage() {
  const marketDataStatus = useAppSelector((state) => state.app.marketDataStatus);

  return (
    <section aria-labelledby="overview-heading">
      <h1 id="overview-heading" className="text-2xl font-semibold">
        Обзор
      </h1>
      <p className="mt-2 text-muted-foreground">
        Статус сервиса данных: {marketDataStatus === "ready" ? "готов" : "неизвестно"}.
        Страницы сетапов, истории и запусков появятся на следующих этапах.
      </p>
    </section>
  );
}
