export interface GithubUser {
  id: string;
  login: string;
  avatarUrl: string;
}

/**
 * Check a user access token against OUR OAuth app (a token from any other app is refused), and return its user.
 * https://docs.github.com/rest/apps/oauth-applications#check-a-token
 */
export async function githubUser(
  token: string,
  clientId: string,
  clientSecret: string,
  doFetch: typeof fetch = fetch,
): Promise<GithubUser | { status: number; error: string }> {
  let res: Response;
  try {
    res = await doFetch(`https://api.github.com/applications/${encodeURIComponent(clientId)}/token`, {
      method: "POST",
      headers: {
        Authorization: `Basic ${btoa(`${clientId}:${clientSecret}`)}`,
        "User-Agent": "indocode-relay",
        Accept: "application/vnd.github+json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ access_token: token }),
    });
  } catch {
    return { status: 502, error: "GitHub is unreachable" };
  }
  if (res.status !== 200) return { status: 401, error: "GitHub rejected the token" };
  const body = (await res.json().catch(() => null)) as { user?: { id?: unknown; login?: unknown; avatar_url?: unknown } } | null;
  const u = body?.user;
  if (!u || !Number.isSafeInteger(u.id) || typeof u.login !== "string" || typeof u.avatar_url !== "string") {
    return { status: 401, error: "GitHub rejected the token" };
  }
  return { id: `gh:${u.id}`, login: u.login, avatarUrl: u.avatar_url };
}
