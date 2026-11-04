import { localTime, localTimezone, type SetupRow } from "./platform-controls";

function SetupDetails({ setup }: { setup: SetupRow }) {
  const metrics = setup.details;
  const ratio = (value: string | null): string => value !== null && value.trim() !== "" && Number.isFinite(Number(value)) ? `${Number(value).toFixed(4)}×` : "нет базы/не определено";
  return <details className="mt-2 min-w-48">
    <summary className="cursor-pointer font-medium">Подробности {setup.symbol}</summary>
    <dl className="mt-2 grid gap-2">
      {setup.algorithmVersion !== undefined && <div><dt className="text-muted-foreground">Версия алгоритма</dt><dd>{setup.algorithmVersion}</dd></div>}
      {metrics ? <>
        <div><dt className="text-muted-foreground">Рост цены (%)</dt><dd>{metrics.growthPct === null ? "не определено" : `${metrics.growthPct}%`}</dd></div>
        <div><dt className="text-muted-foreground">Оборот к длинной базе</dt><dd>{ratio(metrics.volumeLongRatio)}</dd></div>
        <div><dt className="text-muted-foreground">Оборот к короткой базе</dt><dd>{ratio(metrics.volumeShortRatio)}</dd></div>
        <div><dt className="text-muted-foreground">Сработавшая ветвь объёма (OR)</dt><dd>{metrics.overLong && metrics.overShort ? "Обе базы" : metrics.overLong ? "Длинная база" : metrics.overShort ? "Короткая база" : "Ни одна"}</dd></div>
        <div><dt className="text-muted-foreground">Пропущено минут</dt><dd>{metrics.missingMinutes}</dd></div>
      </> : <div><dt className="text-muted-foreground">Метрики сетапа</dt><dd>Не сохранены для этой записи.</dd></div>}
    </dl>
    {setup.params !== undefined && <div className="mt-3"><h3 className="font-medium">Параметры алгоритма</h3><dl className="mt-1 grid gap-1">{Object.entries(setup.params).map(([key, value]) => <div key={key} className="break-words"><dt className="text-muted-foreground">{key}</dt><dd>{value}</dd></div>)}</dl></div>}
  </details>;
}

export function SetupTable({ setups }: { setups: SetupRow[] }) {
  if (!setups.length) return <p className="mt-3 text-muted-foreground">Сетапы не найдены.</p>;
  return <div className="mt-4 overflow-x-auto"><table className="w-full text-sm">
    <caption className="text-left text-muted-foreground">Время: {localTimezone}</caption>
    <thead><tr className="border-b border-input text-left text-muted-foreground"><th className="p-2">Инструмент</th><th className="p-2">Тип</th><th className="p-2">Начало</th><th className="p-2">Закрытие</th><th className="p-2">Опорная цена close</th><th className="p-2">Алгоритм</th><th className="p-2">График</th></tr></thead>
    <tbody>{setups.map((setup, index) => <tr key={`${setup.exchange}-${setup.symbol}-${setup.algorithmId}-${setup.closeTimeUnix}-${index}`} className="border-b border-input"><td className="p-2">{setup.exchange} · {setup.symbol}</td><td className="p-2"><span title="Нейтральное событие наблюдения, не LONG или SHORT">WATCH</span></td><td className="p-2">{localTime(setup.openTimeUnix)}</td><td className="p-2">{localTime(setup.closeTimeUnix)}</td><td className="p-2">{setup.closePrice}</td><td className="p-2">{setup.algorithmId}<SetupDetails setup={setup} /></td><td className="p-2">{setup.exchange === "bybit" && setup.category === "linear" ? <div className="flex flex-wrap gap-2"><a className="underline" href={`https://www.tradingview.com/chart/?symbol=${encodeURIComponent(`BYBIT:${setup.symbol}.P`)}`} target="_blank" rel="noopener noreferrer">TradingView</a><a className="underline" href={`https://www.bybit.com/trade/usdt/${encodeURIComponent(setup.symbol)}`} target="_blank" rel="noopener noreferrer">Bybit</a></div> : <span className="text-muted-foreground">Нет ссылки</span>}</td></tr>)}</tbody>
  </table></div>;
}
