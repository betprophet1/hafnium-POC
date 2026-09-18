// The web's answer to the same question (`storage.ts`): a browser has no keychain, so the session lives in
// localStorage, which is per origin and per browser. It is readable by anything that can run script on this origin,
// which is why the venue's own page is served from the venue's origin and nothing else is let near it (interface 1
// README, browsers).
//
// A browser that keeps nothing — private mode, storage turned off — throws rather than returning null, so every call
// here tolerates that and the app simply asks the member to sign in again.

const KEY = "venue.session";

export async function saveSession(value: string): Promise<void> {
  try {
    globalThis.localStorage?.setItem(KEY, value);
  } catch {
    // Nothing is kept; the session lasts as long as the tab.
  }
}

export async function loadSession(): Promise<string | null> {
  try {
    return globalThis.localStorage?.getItem(KEY) ?? null;
  } catch {
    return null;
  }
}

export async function clearSession(): Promise<void> {
  try {
    globalThis.localStorage?.removeItem(KEY);
  } catch {
    // Already gone as far as anyone can tell.
  }
}
