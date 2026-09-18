// Who is signed in, what the venue says about them, and the stream that keeps it current. One of these for the whole
// app; every screen reads it with `useVenue()`.
//
// The account is not polled. The venue pushes what happens to it — executions, account updates, payouts — and this
// reads the account again when something arrives, which is also what it does when the venue says it could not resume
// (`resync`). That is the whole point of the private stream: a phone does not ask, it is told.

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";

import { Venue } from "./client";
import { openPrivateStream } from "./stream";
import type { Stream } from "./stream";
import { clearSession, loadSession, saveSession } from "./storage";
import type { Account, Member } from "./types";

/** Where the venue is. Set it for a build with EXPO_PUBLIC_VENUE_URL; the default suits the demo on a laptop. */
export const VENUE_URL = process.env.EXPO_PUBLIC_VENUE_URL ?? "http://127.0.0.1:8080";

type VenueState = {
  venue: Venue;
  member: Member | null;
  account: Account | null;
  /** True until the kept session has been read: the app shows nothing rather than flashing a sign-in screen. */
  starting: boolean;
  /** Whether the private stream is connected. */
  live: boolean;
  signUp: (email: string, password: string) => Promise<void>;
  logIn: (email: string, password: string) => Promise<void>;
  logOut: () => Promise<void>;
  /** Read the account again now: after placing an order, or after pulling to refresh. */
  refresh: () => Promise<void>;
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
  const [starting, setStarting] = useState(true);
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

  // A member, from signing up, logging in, or what was kept from last time.
  const start = useCallback(
    async (who: Member) => {
      venue.credential = who.session.credential;
      setMember(who);
      await saveSession(JSON.stringify(who));
      await refresh();
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
    [venue, refresh],
  );

  const forget = useCallback(async () => {
    stream.current?.stop();
    stream.current = null;
    venue.credential = undefined;
    setMember(null);
    setAccount(null);
    setLive(false);
    await clearSession();
  }, [venue]);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const kept = await loadSession();
      if (!cancelled && kept) {
        try {
          await start(JSON.parse(kept) as Member);
        } catch {
          await clearSession();
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

  const state: VenueState = {
    venue,
    member,
    account,
    starting,
    live,
    signUp: async (email, password) => start(await venue.signUp(email, password)),
    logIn: async (email, password) => start(await venue.logIn(email, password)),
    logOut: async () => {
      try {
        await venue.logOut();
      } finally {
        await forget();
      }
    },
    refresh,
  };

  return <Context.Provider value={state}>{children}</Context.Provider>;
}
