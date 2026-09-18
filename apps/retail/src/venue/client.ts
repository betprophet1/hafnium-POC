// Talking to the venue's REST API (interface 1). Nothing here knows about React or a phone: the same client serves
// the app on three platforms and a test in Node, which is where it is tested.
//
// The session credential is the only state it holds. Everything else — what a market is, what an order costs — comes
// from the venue, because the venue is where those rules live.

import { VenueError } from "./types";
import type { Account, Book, Market, Member, OrderAnswer, OrderRequest } from "./types";

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

  /** Signing up. The session it answers with is what the app then holds. */
  async signUp(email: string, password: string, displayName?: string): Promise<Member> {
    const body = displayName ? { email, password, displayName } : { email, password };
    return this.call<Member>("POST", "/v4/members", body, { anonymous: true });
  }

  async logIn(email: string, password: string): Promise<Member> {
    return this.call<Member>("POST", "/v4/sessions", { email, password }, { anonymous: true });
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
  markets: { instrumentId: string; label: string; state: string; outcome?: string | null }[];
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
        outcome: market.outcome ?? null,
      });
    }
  }
  return out;
}
