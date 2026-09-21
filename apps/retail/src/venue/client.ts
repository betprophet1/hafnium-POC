// Talking to the venue's REST API (interface 1). Nothing here knows about React or a phone: the same client serves
// the app on three platforms and a test in Node, which is where it is tested.
//
// The session credential is the only state it holds. Everything else — what a market is, what an order costs — comes
// from the venue, because the venue is where those rules live.

import { VenueError } from "./types";
import type {
  About,
  Account,
  Book,
  CheckYourEmail,
  Environment,
  HeldSession,
  Market,
  Member,
  OrderAnswer,
  OrderRequest,
  ParlayLegInput,
  Rfq,
  RfqStatus,
  TotpStart,
} from "./types";

export type Fetch = typeof globalThis.fetch;

export type ClientOptions = {
  /** Where the venue is: `https://venue.example.com`, with no trailing slash. */
  baseUrl: string;
  /** The session credential, when somebody is signed in. */
  credential?: string;
  /** For tests, and for a platform whose fetch is not the global one. */
  fetch?: Fetch;
};

/** The venue, as this app talks to it. Cheap to make; hold one per signed-in member. */
export class Venue {
  private readonly baseUrl: string;
  private readonly doFetch: Fetch;
  credential?: string;

  constructor(options: ClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.credential = options.credential;
    this.doFetch = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  /** Where the private WebSocket is, for `stream.ts`: the same host, ws or wss as the base URL is http or https. */
  get privateSocketUrl(): string {
    return `${this.baseUrl.replace(/^http/, "ws")}/v4/ws_private`;
  }

  get publicSocketUrl(): string {
    return `${this.baseUrl.replace(/^http/, "ws")}/v4/ws_public`;
  }

  /** Signing up. It answers only that a message went out — never whether the address was already somebody's — so
   * there is no session here: the app asks for the code, then logs in. */
  async signUp(email: string, password: string, displayName?: string): Promise<CheckYourEmail> {
    const body = displayName ? { email, password, displayName } : { email, password };
    return this.call<CheckYourEmail>("POST", "/v4/members", body, { anonymous: true });
  }

  /** The message again, for somebody who has lost it. Answered the same way whoever asks. */
  async resendVerification(email: string): Promise<CheckYourEmail> {
    return this.call<CheckYourEmail>("POST", "/v4/members/verification", { email }, { anonymous: true });
  }

  /** Proving the address with the code from that message. */
  async verifyEmail(token: string): Promise<{ verified: true }> {
    return this.call<{ verified: true }>("POST", "/v4/members/verify", { token }, { anonymous: true });
  }

  async requestReset(email: string): Promise<CheckYourEmail> {
    return this.call<CheckYourEmail>("POST", "/v4/members/reset", { email }, { anonymous: true });
  }

  /** Setting a new password with the code. Every session of that member ends, this one among them. */
  async setPassword(token: string, password: string): Promise<{ passwordSet: true; sessionsEnded: number }> {
    const body = { token, password };
    return this.call<{ passwordSet: true; sessionsEnded: number }>("POST", "/v4/members/password", body, {
      anonymous: true,
    });
  }

  /** Logging in. `code` is the one from an authenticator app, or a recovery code, for an account that asks for one:
   * the venue refuses with `SECOND_FACTOR_REQUIRED` when it wants one and none was sent, which is how the app knows
   * to ask rather than to say the password was wrong. `device` is what this app calls the thing it runs on, so the
   * member can pick it out of their own list of sessions. */
  async logIn(email: string, password: string, code?: string, device?: string): Promise<Member> {
    const body: Record<string, string> = { email, password };
    if (code) body.code = code;
    if (device) body.device = device;
    return this.call<Member>("POST", "/v4/sessions", body, { anonymous: true });
  }

  /** What the venue says about the signed-in member now, which is where the app learns that the address has been
   * proved or that a second factor is on. */
  async about(): Promise<About> {
    return this.call<About>("GET", "/v4/members/me");
  }

  async sessions(): Promise<HeldSession[]> {
    return (await this.call<{ sessions: HeldSession[] }>("GET", "/v4/sessions")).sessions ?? [];
  }

  /** Ending another of this member's sessions: the phone they lost, the browser they left signed in. */
  async endSession(sessionId: string): Promise<unknown> {
    return this.call<unknown>("DELETE", `/v4/sessions/${sessionId}`);
  }

  async startSecondFactor(): Promise<TotpStart> {
    return this.call<TotpStart>("POST", "/v4/members/me/totp");
  }

  /** Confirming it with a code the authenticator showed. The recovery codes are answered once and never again. */
  async confirmSecondFactor(code: string): Promise<{ confirmed: true; recoveryCodes: string[] }> {
    return this.call<{ confirmed: true; recoveryCodes: string[] }>("PUT", "/v4/members/me/totp", { code });
  }

  /** Taking it off, which needs the password: a session somebody else is holding must not be able to. */
  async removeSecondFactor(password: string): Promise<{ removed: true }> {
    return this.call<{ removed: true }>("DELETE", "/v4/members/me/totp", { password });
  }

  /** Ending this session at the venue. The caller forgets the credential either way: a session that could not be
   * ended is still one this device should stop using. */
  async logOut(): Promise<void> {
    try {
      await this.call<unknown>("DELETE", "/v4/sessions/current");
    } finally {
      this.credential = undefined;
    }
  }

  /** Which venues this one says an app may point itself at. Sent with the session when there is one, because a
   * venue names its test environments only to its own people; asked without one before anybody has signed in. Either
   * way it is discovery and not permission — the environment named still decides for itself who may do anything
   * there (interface 1 README, choosing an environment). */
  async bootstrap(): Promise<Environment[]> {
    const answer = await this.call<{ environments: Environment[] }>("GET", "/v4/bootstrap", undefined, {
      anonymous: !this.credential,
    });
    return answer.environments ?? [];
  }

  /** Every market open for trading. Public: no credential needed. */
  async markets(): Promise<Market[]> {
    const answer = await this.call<{ events: EventAnswer[] }>("GET", "/v4/markets?resolved=false", undefined, {
      anonymous: true,
    });
    return marketsOf(answer.events);
  }

  async book(instrumentId: string, depth = 10): Promise<Book> {
    return this.call<Book>("GET", `/v4/markets/${instrumentId}/book?depth=${depth}`, undefined, { anonymous: true });
  }

  async account(): Promise<Account> {
    return this.call<Account>("GET", "/v4/account");
  }

  async placeOrder(order: OrderRequest): Promise<OrderAnswer> {
    return this.call<OrderAnswer>("POST", "/v4/orders", order);
  }

  async cancelOrder(orderId: string): Promise<unknown> {
    return this.call<unknown>("DELETE", `/v4/orders/${orderId}`);
  }

  /** Asks for quotes on a parlay of these legs: two to six of them, one per event, each on an open market.
   *
   * The answer names the parlay the venue listed for the legs and says how many responders the request reached.
   * Nothing is bought here and nothing is owed — a request is an invitation, and `responders: 0` means nobody was
   * listening, so no quote is coming however long the window runs. */
  async requestQuote(legs: ParlayLegInput[], quantity: number): Promise<Rfq> {
    return this.call<Rfq>("POST", "/v4/rfqs", { legs, quantity });
  }

  /** A request and the quotes standing against it now. Asked for again while the window is open: quotes arrive
   * over the private stream too, but this app reads the book by asking, as the market screen does. */
  async rfq(requestId: string): Promise<RfqStatus> {
    return this.call<RfqStatus>("GET", `/v4/rfqs/${requestId}`);
  }

  /** A deposit through the treasury's fake bank, which is how a test environment gets money into an account. */
  async deposit(cents: number, method: string): Promise<unknown> {
    return this.call<unknown>("POST", "/v4/transfers", { direction: "deposit", amountCents: cents, method });
  }

  private async call<T>(
    method: string,
    path: string,
    body?: unknown,
    options: { anonymous?: boolean } = {},
  ): Promise<T> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (!options.anonymous) {
      if (!this.credential) throw new VenueError("UNAUTHORIZED", "sign in first", 401);
      headers.Authorization = `Bearer ${this.credential}`;
    }
    let response: Response;
    try {
      response = await this.doFetch(`${this.baseUrl}${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch (e) {
      // The venue was not reached at all: nothing was done, and trying again may work.
      throw new VenueError("UNREACHABLE", `the venue could not be reached: ${String(e)}`, 503);
    }
    const text = await response.text();
    const answer: unknown = text ? safeJson(text) : null;
    if (!response.ok) {
      const error = (answer as { error?: { code?: string; message?: string } } | null)?.error;
      const retry = response.headers.get("Retry-After");
      throw new VenueError(
        error?.code ?? "ERROR",
        error?.message ?? `the venue answered ${response.status}`,
        response.status,
        retry ? Number(retry) : undefined,
      );
    }
    return answer as T;
  }
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

/** What `GET /v4/markets` answers: events, each with its markets. The app shows markets, so they are flattened with
 * the event's title kept, which is what a person reads. */
type EventAnswer = {
  eventId: string;
  title: string;
  markets: { instrumentId: string; label: string; class?: string; state: string; outcome?: string | null }[];
};

export function marketsOf(events: EventAnswer[]): Market[] {
  const out: Market[] = [];
  for (const event of events ?? []) {
    for (const market of event.markets ?? []) {
      out.push({
        instrumentId: market.instrumentId,
        eventId: event.eventId,
        label: market.label,
        title: event.title,
        state: market.state,
        // The API makes `class` required, but an older venue may not send it; "binary" is the ordinary kind and the
        // only consequence of guessing it here is that such a market stays offerable as a parlay leg.
        class: market.class ?? "binary",
        outcome: market.outcome ?? null,
      });
    }
  }
  return out;
}
