// Where a signed-in session is kept between launches, on a phone: the keychain on iOS, the keystore on Android.
//
// This is one of the few places the three platforms really differ, and the difference is a file: `storage.web.ts`
// stands in for this one when the app is built for the web, because a browser has no keychain. Everything else in
// the app is the same code on all three.

import * as SecureStore from "expo-secure-store";

const KEY = "venue.session";

export async function saveSession(value: string): Promise<void> {
  await SecureStore.setItemAsync(KEY, value);
}

export async function loadSession(): Promise<string | null> {
  return SecureStore.getItemAsync(KEY);
}

export async function clearSession(): Promise<void> {
  await SecureStore.deleteItemAsync(KEY);
}
