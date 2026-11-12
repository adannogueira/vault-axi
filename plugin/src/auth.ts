// Hosted-cloud session handling (Supabase auth), mirroring the CLI's `vault login` and
// `VAULT_API_KEY` exchange.
import { describe, isOk, type Http } from "./http";
import { SyncError } from "./remote";

export const HOSTED_URL = "https://mlmmzutdfnahbdquslzf.supabase.co";
/** Public anon key; every row is protected by RLS `auth.uid() = user_id`. */
export const HOSTED_ANON_KEY =
  "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6Im1sbW16dXRkZm5haGJkcXVzbHpmIiwicm9sZSI6ImFub24iLCJpYXQiOjE3OTM0NTcyNTUsImV4cCI6MjEwOTAzMzI1NX0.5ac5z0VZi1ZWTG9eyd8CtMVawP1mlL4Sxf7cRKTSSNc";

export interface CloudSession {
  accessToken: string;
  refreshToken: string | null;
  userId: string | null;
  email: string | null;
  vaultId: string;
  /** `read` tokens can pull but never push. */
  scope: string;
  /** A `vlt_` API token re-exchanged when the JWT expires. */
  apiToken: string | null;
}

type Json = Record<string, unknown>;

function str(v: unknown): string | null {
  return typeof v === "string" && v !== "" ? v : null;
}

async function post(http: Http, path: string, body: Json, bearer?: string): Promise<Json> {
  const headers: Record<string, string> = { apikey: HOSTED_ANON_KEY, "Content-Type": "application/json" };
  if (bearer) headers.Authorization = `Bearer ${bearer}`;
  const res = await http({ url: `${HOSTED_URL}${path}`, method: "POST", headers, body: JSON.stringify(body) });
  if (!isOk(res)) throw new SyncError("AUTH_ERROR", `Sign-in failed (${describe(res)})`);
  return JSON.parse(res.text) as Json;
}

async function verifyTotp(http: Http, accessToken: string, code: string): Promise<{ access: string; refresh: string | null }> {
  const userRes = await http({
    url: `${HOSTED_URL}/auth/v1/user`,
    method: "GET",
    headers: { apikey: HOSTED_ANON_KEY, Authorization: `Bearer ${accessToken}` },
  });
  if (!isOk(userRes)) throw new SyncError("AUTH_ERROR", `Fetching MFA factors failed (${describe(userRes)})`);
  const factors = (JSON.parse(userRes.text) as { factors?: { id?: string }[] }).factors ?? [];
  const factorId = factors[0]?.id;
  if (!factorId) throw new SyncError("AUTH_ERROR", "No TOTP factor is enrolled on this account");
  const challenge = await post(http, `/auth/v1/factors/${factorId}/challenge`, {}, accessToken);
  const verified = await post(
    http,
    `/auth/v1/factors/${factorId}/verify`,
    { challenge_id: str(challenge.id), code },
    accessToken,
  );
  const access = str(verified.access_token);
  if (!access) throw new SyncError("AUTH_ERROR", "MFA verification returned no session");
  return { access, refresh: str(verified.refresh_token) };
}

export async function passwordLogin(
  http: Http,
  email: string,
  password: string,
  totpCode: string | null,
  vaultId: string,
): Promise<CloudSession> {
  const body = await post(http, "/auth/v1/token?grant_type=password", { email, password });
  let accessToken = str(body.access_token);
  let refreshToken = str(body.refresh_token);
  if (!accessToken) throw new SyncError("AUTH_ERROR", "Sign-in returned no session");
  if (totpCode) {
    const aal2 = await verifyTotp(http, accessToken, totpCode);
    accessToken = aal2.access;
    refreshToken = aal2.refresh ?? refreshToken;
  }
  const user = body.user as Json | undefined;
  return {
    accessToken,
    refreshToken,
    userId: str(user?.id),
    email,
    vaultId: vaultId || "default",
    scope: "write",
    apiToken: null,
  };
}

/** Exchange a `vlt_` API token (from `vault token create`) for a session. */
export async function exchangeApiToken(http: Http, apiToken: string): Promise<CloudSession> {
  const body = await post(http, "/functions/v1/token-exchange", { action: "exchange", api_key: apiToken.trim() });
  const accessToken = str(body.access_token);
  if (!accessToken) throw new SyncError("AUTH_ERROR", "Token exchange returned no session");
  return {
    accessToken,
    refreshToken: str(body.refresh_token),
    userId: str(body.user_id),
    email: str(body.email),
    vaultId: str(body.vault_id) ?? "default",
    scope: str(body.scope) ?? "write",
    apiToken: apiToken.trim(),
  };
}

/** A fresh session for an expired one, or `null` when it can't be renewed. */
export async function renewSession(http: Http, session: CloudSession): Promise<CloudSession | null> {
  try {
    if (session.apiToken) {
      const fresh = await exchangeApiToken(http, session.apiToken);
      return { ...fresh, vaultId: session.vaultId };
    }
    if (session.refreshToken) {
      const body = await post(http, "/auth/v1/token?grant_type=refresh_token", { refresh_token: session.refreshToken });
      const accessToken = str(body.access_token);
      if (!accessToken) return null;
      return { ...session, accessToken, refreshToken: str(body.refresh_token) ?? session.refreshToken };
    }
  } catch {
    return null;
  }
  return null;
}
