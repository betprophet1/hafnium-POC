// Who is signed in, what the venue says about them, and the stream that keeps it current. One of these for the whole
// app; every screen reads it with `useVenue()`.
//
// The account is not polled. The venue pushes what happens to it — executions, account updates, payouts — and this
// reads the account again when something arrives, which is also what it does when the venue says it could not resume
// (`resync`). That is the whole point of the private stream: a phone does not ask, it is told.

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import { Platform } from "react-native";

import { Venue } from "./client";
import { openPrivateStream } from "./stream";
import type { Stream } from "./stream";
import { clearSession, keptSession, loadSession, saveSession, unlockKind, unlockName } from "./storage";
import type { Unlock } from "./storage";
import type { About, Account, HeldSession, Member } from "./types";

/** Where the venue is. Set it for a build with EXPO_PUBLIC_VENUE_URL; the default suits the demo on a laptop. */
export const VENUE_URL = process.env.EXPO_PUBLIC_VENUE_URL ?? "http://127.0.0.1:8080";

/** What this app calls the thing it runs on, so a member can pick this session out of their own list. A name, not a
 * fingerprint of the device: the venue is told "an iPhone", never anything that identifies one. */
function deviceName(): string {
  const platform = Platform.OS;
  if (platform === "ios") return "an iPhone";
  if (platform === "android") return "an Android phone";
  if (platform === "web") return "a browser";
  return "an app";
}

type VenueState = {
  venue: Venue;
  member: Member | null;
  account: Account | null;
  /** True until the kept session has been read: the app shows nothing rather than flashing a sign-in screen. */
  starting: boolean;
  /** Whether the private stream is connected. */
  live: boolean;
  /** What the venue last said about this member: whether the address is proved, whether a second factor is on. */
  about: About | null;
  /** There is a session kept on this device behind the phone's lock, waiting to be unlocked. */
  locked: boolean;
  /** What this device can unlock with, and what to call it on a button. */
  unlock: Unlock;
  unlockWith: string;
  /** Signing up answers nothing but "check your mail": no session, so the app asks for the code and then logs in. */
  signUp: (email: string, password: string) => Promise<void>;
  /** `code` is from an authenticator app or a recovery code, for an account that asks for one. The venue refuses
   * with `SECOND_FACTOR_REQUIRED` when it wants one and none was sent, which is how a screen knows to ask. */
  logIn: (email: string, password: string, code?: string) => Promise<void>;
  /** Unlocks the session this device is holding, asking for the fingerprint. False if there was nothing to unlock,
   * or the member said no, or the phone's enrolled biometrics have changed since it was kept. */
  unlockSession: () => Promise<boolean>;
  logOut: () => Promise<void>;
  /** Read the account again now: after placing an order, or after pulling to refresh. */
  refresh: () => Promise<void>;
  /** Read what the venue says about the member again: after proving an address, or changing a second factor. */
  refreshAbout: () => Promise<void>;
  sessions: () => Promise<HeldSession[]>;
};

const Context = createContext<VenueState | null>(null);

export function useVenue(): VenueState {
  const state = useContext(Context);
  if (!state) throw new Error("useVenue outside VenueProvider");
  return state;
}

export function VenueProvider({ children }: { children: ReactNode }) {
  const venue = useMemo(() => new Venue({ baseUrl: VENUE_URL }), []);
  const [member, setMember] = useState<Member | null>(null);
  const [account, setAccount] = useState<Account | null>(null);
  const [about, setAbout] = useState<About | null>(null);
  const [starting, setStarting] = useState(true);
  const [locked, setLocked] = useState(false);
  const [unlock, setUnlock] = useState<Unlock>("none");
  const [unlockWith, setUnlockWith] = useState("the phone's lock");
  const [live, setLive] = useState(false);
  const stream = useRef<Stream | null>(null);

  const refresh = useCallback(async () => {
    if (!venue.credential) return;
    try {
      setAccount(await venue.account());
    } catch {
      // The stream will bring it back; a failed read is not worth showing over the last known state.
    }
  }, [venue]);

  const refreshAbout = useCallback(async () => {
    if (!venue.credential) return;
    try {
      setAbout(await venue.about());
    } catch {
      // Same: what is on screen is the last thing the venue said, which is better than nothing.
    }
  }, [venue]);

  // A member, from signing up, logging in, or what was kept from last time.
  const start = useCallback(
    async (who: Member) => {
      venue.credential = who.session.credential;
      setMember(who);
      setLocked(false);
      await saveSession(JSON.stringify(who));
      await Promise.all([refresh(), refreshAbout()]);
      stream.current?.stop();
      stream.current = openPrivateStream({
        url: venue.privateSocketUrl,
        credential: who.session.credential,
        handlers: {
          onOpen: () => {
            setLive(true);
            void refresh();
          },
          onClosed: () => setLive(false),
          onMessage: () => void refresh(),
          onError: (e) => {
            // A session the venue no longer accepts: sign out rather than retrying for ever.
            if (e.code === "UNAUTHORIZED") void forget();
          },
        },
      });
    },
    // `forget` is defined below and never changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [venue, refresh, refreshAbout],
  );

  const forget = useCallback(async () => {
    stream.current?.stop();
    stream.current = null;
    venue.credential = undefined;
    setMember(null);
    setAccount(null);
    setAbout(null);
    setLocked(false);
    setLive(false);
    await clearSession();
  }, [venue]);

  /** The kept session, behind the phone's lock. Called from a button, never at launch: a prompt nobody asked for is
   * the fastest way to teach people to dismiss it. */
  const unlockSession = useCallback(async () => {
    const kept = await loadSession();
    if (!kept) {
      setLocked(false);
      return false;
    }
    try {
      await start(JSON.parse(kept) as Member);
      return true;
    } catch {
      await clearSession();
      setLocked(false);
      return false;
    }
  }, [start]);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const [kind, name, kept] = await Promise.all([unlockKind(), unlockName(), keptSession()]);
      if (cancelled) return;
      setUnlock(kind);
      setUnlockWith(name);
      // A session kept behind the lock is not read here: the launch draws a screen offering to unlock it, and the
      // member asks for the prompt. One kept without a lock — a phone with nothing enrolled, or the web — is read
      // straight away, which is what it was before there was a lock at all.
      if (kept.kept && !kept.locked) {
        const session = await loadSession();
        if (!cancelled && session) {
          try {
            await start(JSON.parse(session) as Member);
          } catch {
            await clearSession();
          }
        }
      } else if (!cancelled) {
        setLocked(kept.kept);
      }
      if (!cancelled) setStarting(false);
    })();
    return () => {
      cancelled = true;
      stream.current?.stop();
    };
    // Once, at the start.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const state: VenueState = {
    venue,
    member,
    account,
    about,
    starting,
    locked,
    unlock,
    unlockWith,
    live,
    signUp: async (email, password) => {
      await venue.signUp(email, password);
    },
    logIn: async (email, password, code) => start(await venue.logIn(email, password, code, deviceName())),
    unlockSession,
    logOut: async () => {
      try {
        await venue.logOut();
      } finally {
        await forget();
      }
    },
    refresh,
    refreshAbout,
    sessions: () => venue.sessions(),
  };

  return <Context.Provider value={state}>{children}</Context.Provider>;
}
