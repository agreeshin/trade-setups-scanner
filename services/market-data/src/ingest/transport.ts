export interface TransportResponse {
  status: number;
  headers: Map<string, string>;
  bodyText: string;
}

export interface Transport {
  fetch(url: string): Promise<TransportResponse>;
}

export class FetchTransport implements Transport {
  async fetch(url: string): Promise<TransportResponse> {
    const response = await fetch(url);
    const headers = new Map<string, string>();
    for (const [key, value] of response.headers.entries()) {
      headers.set(key.toLowerCase(), value);
    }
    return {
      status: response.status,
      headers,
      bodyText: await response.text(),
    };
  }
}
