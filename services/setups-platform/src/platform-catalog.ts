export interface CatalogInstrument {
  id: number;
  exchange: string;
  category: string;
  symbol: string;
  status: string;
}

export async function fetchCatalog(
  marketDataUrl: string,
  apiKey: string,
  signal?: AbortSignal,
): Promise<CatalogInstrument[]> {
  const response = await fetch(`${marketDataUrl}/v1/instruments`, {
    headers: { "x-api-key": apiKey },
    signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(10000)]) : AbortSignal.timeout(10000),
  });
  if (!response.ok) {
    throw new Error(
      `market data catalog ${response.status}: ${(await response.text()).slice(0, 200)}`,
    );
  }
  const body = (await response.json()) as { instruments: CatalogInstrument[] };
  return body.instruments;
}
