export async function retryWithBackoff<T>(fn: () => Promise<T>, maxAttempts = 3): Promise<T> {
  let attempt = 0;
  let lastError: unknown;
  while (attempt < maxAttempts) {
    try {
      return await fn();
    } catch (err) {
      lastError = err;
      attempt += 1;
      await new Promise((resolve) => setTimeout(resolve, 100 * attempt));
    }
  }
  throw lastError;
}

export async function fetchData(url: string): Promise<string> {
  return retryWithBackoff(async () => {
    const resp = await fetch(url, { cache: "no-store" });
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    return await resp.text();
  });
}

export function parseJson<T>(raw: string): T {
  return JSON.parse(raw) as T;
}
