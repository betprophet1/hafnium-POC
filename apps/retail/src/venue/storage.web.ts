// The web's answer to the same question (`storage.ts`): a browser has no keychain and no fingerprint reader, so the
// session lives in localStorage, which is per origin and per browser. It is readable by anything that can run script
// on this origin, which is why the venue's own page is served from the venue's origin and nothing else is let near
// it (interface 1 README, browsers).
//
// A browser that keeps nothing — private mode, storage turned off — throws rather than returning null, so every call
// here tolerates that and the app simply asks the member to sign in again.
//
// There is no web equivalent of the phone's fingerprint unlock, and this file does not pretend there is: `unlockKind`
// answers "none", so every screen that would offer it simply does not, and the same code runs on all three platforms.

const KEY = "venue.session";

export type Unlock = "biometric" | "passcode" | "none";

export async function unlockKind(): Promise<Unlock> {
  return "none";
}

export async function unlockName(): Promise<string> {
  return "the phone's lock";
}

export async function saveSession(value: string): Promise<void> {
  try {
    globalThis.localStorage?.setItem(KEY, value);
  } catch {
    // Nothing is kept; the session lasts as long as the tab.
  }
}

export async function keptSession(): Promise<{ kept: boolean; locked: boolean }> {
  try {
    return { kept: globalThis.localStorage?.getItem(KEY) != null, locked: false };
  } catch {
    return { kept: false, locked: false };
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
