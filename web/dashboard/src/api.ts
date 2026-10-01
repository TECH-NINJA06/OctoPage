// The service's API, as the dashboard calls it: same origin, signed in with the session
// cookie. Values and errors as the API documents them (crates/octopage-server/src/api.rs).

export type SqlValue = null | number | bigint | string | Uint8Array;

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

/** Called when a request finds the session gone, so the app can show the sign-in page. */
let onSignedOut: () => void = () => {};
export const whenSignedOut = (handler: () => void) => {
  onSignedOut = handler;
};

export async function api<T = any>(method: string, path: string, body?: unknown): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      method,
      credentials: 'same-origin',
      headers: body === undefined ? {} : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch (error) {
    throw new ApiError(0, 'network', `The service did not answer (${(error as Error).message}).`);
  }
  const text = await response.text();
  let json: any = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    // Not the API's JSON: reported by status below.
  }
  if (!response.ok) {
    if (response.status === 401) onSignedOut();
    throw new ApiError(
      response.status,
      json?.error?.code ?? 'http',
      json?.error?.message ?? `The service answered ${response.status}.`,
    );
  }
  return json as T;
}

function fromBase64(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

export function decode(json: unknown): SqlValue {
  if (json === null || typeof json === 'number' || typeof json === 'string') return json;
  const tagged = json as Record<string, string>;
  if (typeof tagged?.$blob === 'string') return fromBase64(tagged.$blob);
  if (typeof tagged?.$int === 'string') return BigInt(tagged.$int);
  if (typeof tagged?.$real === 'string') {
    const r = tagged.$real;
    return r === 'NaN' ? NaN : r === 'inf' ? Infinity : r === '-inf' ? -Infinity : Number(r);
  }
  return String(json);
}

export interface Result {
  columns: string[];
  rows: SqlValue[][];
  changed: number;
  commit: string | null;
}

export const result = (json: any): Result => ({
  columns: json?.columns ?? [],
  rows: (json?.rows ?? []).map((row: unknown[]) => row.map(decode)),
  changed: json?.changed ?? 0,
  commit: json?.commit ?? null,
});

// ------------------------------------------------------------------ shapes

export interface Me {
  login: string;
  session: boolean;
  installations: { id: number; account: string; suspended: boolean }[];
  install_url: string | null;
  /** Whether the service can hold databases' keys itself. */
  kms: boolean;
}

export interface DatabaseInfo {
  id: string;
  repository: string;
  branch: string;
  encryption: 'none' | 'kms' | 'passphrase';
  created: number;
  new?: boolean;
  head?: string;
  page_size?: number;
  retention?: string;
  recovery_key?: string;
}

export interface Commit {
  commit: string;
  parent: string | null;
  time: number;
  message: string;
  statements: { sql: string; params: unknown[] }[];
}

export interface Settings {
  retention: string;
  snapshot_days: number;
  live_limit: number;
  repository_budget: number;
  warn_days: number;
  grace_days: number;
}

export interface Stats {
  pages: number;
  page_size: number;
  live_bytes: number;
  live_limit: number;
  commits_30d: number;
  bytes_per_day: number;
  repository_bytes: number | null;
  repository_budget: number;
  days_left: number | null;
  warn: boolean;
  measured: number;
}

export interface Key {
  id: string;
  name: string;
  prefix: string;
  created: number;
  last_used: number | null;
}

export interface UsageDay {
  day: number;
  requests: number;
  github_requests: number;
  commits: number;
}

// ------------------------------------------------------------------ formatting

/** `refs/heads/main` → `main`. */
export const branchName = (ref: string) => ref.replace(/^refs\/heads\//, '');

export const shortSha = (sha: string | null | undefined) => (sha ?? '').slice(0, 10);

export function bytes(n: number | null | undefined): string {
  if (n === null || n === undefined) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value >= 100 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

export const count = (n: number) => new Intl.NumberFormat(undefined, { notation: n >= 10_000 ? 'compact' : 'standard' }).format(n);

export function when(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  });
}

export function ago(seconds: number): string {
  const delta = Date.now() / 1000 - seconds;
  const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });
  if (delta < 60) return rtf.format(-Math.round(delta), 'second');
  if (delta < 3600) return rtf.format(-Math.round(delta / 60), 'minute');
  if (delta < 86_400) return rtf.format(-Math.round(delta / 3600), 'hour');
  return rtf.format(-Math.round(delta / 86_400), 'day');
}

/** A value as a result cell shows it. */
export function display(value: SqlValue): string {
  if (value === null) return 'NULL';
  if (value instanceof Uint8Array) {
    const hex = Array.from(value.subarray(0, 32), (b) => b.toString(16).padStart(2, '0')).join('');
    return `x'${hex}${value.length > 32 ? '…' : ''}' (${value.length} bytes)`;
  }
  return String(value);
}

/** An SQL identifier, quoted. */
export const ident = (name: string) => `"${name.replaceAll('"', '""')}"`;

/** An SQL string literal. */
export const literal = (text: string) => `'${text.replaceAll("'", "''")}'`;
