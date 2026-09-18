// Where a signed-in session is kept between launches, on a phone: the keychain on iOS, the keystore on Android.
//
// This is one of the few places the three platforms really differ, and the difference is a file: `storage.web.ts`
// stands in for this one when the app is built for the web, because a browser has neither a keychain nor a
// fingerprint reader. Everything else in the app is the same code on all three.
//
// **What the fingerprint does, and what it does not.** It unlocks the credential this device is holding. It is not
// how the venue knows who the member is — the venue only ever sees the session credential, exactly as it would if
// the member had typed their password. A fingerprint here is the phone standing between someone holding the unlocked
// device and the session inside it; the venue is not told that a fingerprint was used and does not treat the session
// differently for it. Anything the venue must be sure of belongs in the second factor instead (interface 1 README,
// members and sessions).
//
// **It can stop working, by design.** A keystore entry that asks for biometrics is invalidated when the set of
// enrolled fingerprints or faces changes — a new fingerprint added, all of them removed — because the phone can no
// longer say the same person is asking. The read then fails, and the only honest answer is to forget the entry and
// have the member sign in with their password again. Every read here treats a failure that way rather than retrying.

import * as LocalAuthentication from "expo-local-authentication";
import * as SecureStore from "expo-secure-store";

const KEY = "venue.session";
/** Set beside it, without the lock, so the app can say "unlock to sign in" without first asking for a fingerprint. */
const MARKER = "venue.session.kept";

/** What this device can do about unlocking, so a screen can say the right thing. */
export type Unlock = "biometric" | "passcode" | "none";

/** Whether the phone has a reader, an enrolled fingerprint or face, and so something to unlock with. `passcode`
 * means it can check the screen lock but has nothing enrolled; `none` means it cannot check anything, and the
 * session is kept without a lock in front of it. */
export async function unlockKind(): Promise<Unlock> {
  try {
    if (!(await LocalAuthentication.hasHardwareAsync())) return "none";
    if (await LocalAuthentication.isEnrolledAsync()) return "biometric";
    const levels = await LocalAuthentication.getEnrolledLevelAsync();
    return levels === LocalAuthentication.SecurityLevel.SECRET ? "passcode" : "none";
  } catch {
    return "none";
  }
}

/** How the reader is named on this phone, for a button that has to say what it will ask for. */
export async function unlockName(): Promise<string> {
  try {
    const kinds = await LocalAuthentication.supportedAuthenticationTypesAsync();
    if (kinds.includes(LocalAuthentication.AuthenticationType.FACIAL_RECOGNITION)) return "your face";
    if (kinds.includes(LocalAuthentication.AuthenticationType.FINGERPRINT)) return "your fingerprint";
    if (kinds.includes(LocalAuthentication.AuthenticationType.IRIS)) return "your eye";
  } catch {
    // Fall through to the general word.
  }
  return "the phone's lock";
}

export async function saveSession(value: string): Promise<void> {
  const locked = (await unlockKind()) !== "none";
  await SecureStore.setItemAsync(KEY, value, locked ? { requireAuthentication: true } : {});
  await SecureStore.setItemAsync(MARKER, locked ? "locked" : "open");
}

/** Whether there is a session kept here, and whether reading it will ask for a fingerprint. Costs no prompt, so a
 * launch can draw the right screen before anything is asked of the member. */
export async function keptSession(): Promise<{ kept: boolean; locked: boolean }> {
  try {
    const marker = await SecureStore.getItemAsync(MARKER);
    return { kept: marker !== null, locked: marker === "locked" };
  } catch {
    return { kept: false, locked: false };
  }
}

/** The kept session, asking for the fingerprint if one was used to keep it. Null when there is none, when the member
 * refused the prompt, or when the entry has been invalidated because the enrolled biometrics changed — in the last
 * case what is left behind is cleared, because it can never be read again. */
export async function loadSession(): Promise<string | null> {
  const { kept, locked } = await keptSession();
  if (!kept) return null;
  try {
    return await SecureStore.getItemAsync(KEY, locked ? { requireAuthentication: true } : {});
  } catch {
    // Either the member said no, or the key is gone for good. Telling those apart is not worth the guessing: the
    // next screen asks for the password, and a member who meant to unlock can try again from there.
    await clearSession();
    return null;
  }
}

export async function clearSession(): Promise<void> {
  await SecureStore.deleteItemAsync(KEY).catch(() => {});
  await SecureStore.deleteItemAsync(MARKER).catch(() => {});
}
