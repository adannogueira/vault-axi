export interface HttpRequest {
  url: string;
  method: string;
  headers?: Record<string, string>;
  body?: string;
}

export interface HttpResponse {
  status: number;
  /** Header names are lower-cased. */
  headers: Record<string, string>;
  text: string;
}

/** Never throws on HTTP error statuses; only on transport failures. */
export type Http = (request: HttpRequest) => Promise<HttpResponse>;

export function lowerCaseHeaders(headers: Record<string, string>): Record<string, string> {
  return Object.fromEntries(Object.entries(headers).map(([k, v]) => [k.toLowerCase(), v]));
}

export function isOk(response: HttpResponse): boolean {
  return response.status >= 200 && response.status < 300;
}

export function describe(response: HttpResponse): string {
  let detail = "";
  try {
    const body = JSON.parse(response.text) as Record<string, unknown>;
    const msg = body.msg ?? body.error_description ?? body.message ?? body.error;
    if (typeof msg === "string") detail = `: ${msg}`;
  } catch {
    // not JSON
  }
  return `HTTP ${response.status}${detail}`;
}
