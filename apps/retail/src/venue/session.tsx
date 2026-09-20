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
import { BOOTSTRAP_URL, BUILT_IN, fallback, readKept, stillOffered, withBuiltIn } from "./environment";
import { clearSession, keptSession, loadSession, saveSession, unlockKind, unlockName } from "./storage";
import type { Unlock } from "./storage";
import type { About, Account, Environment, HeldSession, Member } from "./types";

export { BOOTSTRAP_URL } from "./environment";

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
  /** The venue this app is pointed at, and the ones it has been told it may point itself at. */
  environment: Environment;
  environments: Environment[];
  /** Points the app at another venue. Signs out first: a session belongs to the venue that issued it and is nobody
   * else's, so there is nothing to carry across (decision G-20). */
  chooseEnvironment: (to: Environment) => Promise<void>;
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
  // Which venue, and therefore which client. Rebuilt when the target moves, because a Venue is bound to one address
  // and a session is bound to one venue.
  const [environment, setEnvironment] = useState<Environment>(BUILT_IN);
  const [environments, setEnvironments] = useState<Environment[]>([]);
  const venue = useMemo(() => new Venue({ baseUrl: environment.url }), [environment.url]);
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
      // The venue goes in with it: restoring a session against the wrong one would send a credential somewhere it
      // does not belong, which is the one thing that must never happen here.
      await saveSession(JSON.stringify({ environment, member: who }));
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
    [venue, environment, refresh, refreshAbout],
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
    const kept = readKept(await loadSession());
    if (!kept) {
      setLocked(false);
      return false;
    }
    try {
      // Back to the venue it came from, not wherever the app happens to be pointed now.
      setEnvironment(kept.environment);
      await start(kept.member as Member);
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
        const held = readKept(await loadSession());
        if (!cancelled && held) {
          // Asked of the venue the session belongs to, carrying that session, because that is the only venue that
          // can say whether it is still good and the only one entitled to see it. If the venue has gone — a preview
          // environment outlives no pull request — this throws and the app falls back to where it was built
          // pointing, rather than retrying an address that no longer answers.
          try {
            const there = new Venue({ baseUrl: held.environment.url, credential: (held.member as Member).session.credential });
            const offered = await there.bootstrap();
            if (!cancelled && stillOffered(held, offered)) {
              setEnvironments(withBuiltIn(offered));
              setEnvironment(held.environment);
              await start(held.member as Member);
              if (!cancelled) setStarting(false);
              return;
            }
          } catch {
            // Fall through to starting afresh against the built-in address.
          }
          await clearSession();
        }
      } else if (!cancelled) {
        setLocked(kept.kept);
      }
      // Nobody signed in, or what was kept is no good: ask the address the app was built with where it may go.
      if (!cancelled) {
        try {
          const offered = await new Venue({ baseUrl: BOOTSTRAP_URL }).bootstrap();
          if (!cancelled && offered.length) {
            setEnvironments(withBuiltIn(offered));
            setEnvironment(fallback(offered));
          }
        } catch {
          // A venue that does not offer the route, or cannot be reached: the app stays where it was built pointing.
        }
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

  /** Points the app at another venue. Signing out first is not tidiness: the session belongs to the venue that
   * issued it, so carrying it across would mean handing a credential to a venue it is not for. */
  const chooseEnvironment = useCallback(
    async (to: Environment) => {
      if (to.url === environment.url) return;
      try {
        await venue.logOut();
      } catch {
        // The old venue could not be told. The credential is dropped here regardless, which is what matters.
      }
      await forget();
      setEnvironment(to);
    },
    // `forget` never changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [venue, environment.url],
  );

  // Once somebody is signed in, ask again: a venue names its test environments only to its own people, so the list
  // an app was given before anyone signed in is the short one.
  useEffect(() => {
    if (!member) return;
    let cancelled = false;
    void (async () => {
      try {
        const offered = await venue.bootstrap();
        if (!cancelled && offered.length) setEnvironments(withBuiltIn(offered));
      } catch {
        // Keep the list already held; it is only ever used to offer a choice.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [member, venue]);

  const state: VenueState = {
    venue,
    member,
    account,
    about,
    starting,
    locked,
    unlock,
    unlockWith,
    environment,
    environments,
    chooseEnvironment,
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
