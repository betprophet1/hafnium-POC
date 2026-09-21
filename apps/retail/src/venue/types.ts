// What the venue says, in the words interface 1 uses (interfaces/gateway/README.md). Only the parts this app reads
// are here; the gateway sends more, and anything not named below is ignored rather than guessed at.
//
// Identifiers are strings everywhere, as the API sends them: they are 64-bit numbers that JSON cannot hold exactly.
// Money is whole cents, never a float. A price is a tick on the market's ladder, with the same tick shown four ways.

/** A price, as every answer carries it: the tick that names it, and what that tick is worth. */
export type Price = {
  /** The rung of the market's ladder. What an order names. */
  tick: number;
  /** The price in cents, as text: "2" is two cents for a contract that pays a dollar. */
  cents: string;
  /** The same as a probability, "0.0400". */
  probability: string;
  /** And as American odds, "+2400". */
  american: string;
};

export type Side = "buy" | "sell";
export type Outcome = "yes" | "no";

/** One rung of a book, as the public stream sends it. */
export type Level = { price: Price; quantity: number; orderCount: number };

export type Book = { instrumentId: string; bids: Level[]; asks: Level[]; sequence: string };

/** A market: one thing that can happen, with a book of its own. */
export type Market = {
  instrumentId: string;
  eventId: string;
  label: string;
  title: string;
  state: string;
  /** What kind of market it is. `parlay` is one the venue listed to answer a request for quote, and it cannot be a
   * leg of another: a parlay of parlays is refused (core/parlays/src/rules.rs, canonical). */
  class: string;
  /** Set once the market has resolved, and then the book is gone. */
  outcome?: string | null;
};

/** What the account is worth and what it holds, from GET /v4/account. */
export type Account = {
  accountId: string;
  availableCents: number;
  reservedCents: number;
  positions: Position[];
  orders: RestingOrder[];
  sequence: string;
};

export type Position = { instrumentId: string; quantity: number };

export type RestingOrder = {
  orderId: string;
  instrumentId: string;
  side: Side;
  price: Price;
  remaining: number;
  clientOrderId: string;
};

/** A member and the session they hold, from signing up or logging in. */
export type Member = {
  memberId: string;
  accountId: string;
  email: string;
  displayName: string;
  /** Whether the address has been proved. Until it is, this session may trade but not move money. */
  emailVerified: boolean;
  session: Session;
};

export type Session = { sessionId: string; credential: string; expiresAt: string };

/** A venue an app may point itself at, as `GET /v4/bootstrap` names it. `live` says whose money is real; an app
 * shows that where a person can see it, and an entry that does not say is taken as not live. */
export type Environment = {
  id: string;
  label: string;
  url: string;
  live: boolean;
};

/** What the member's own routes say about them, which `GET /v4/members/me` answers. */
export type About = {
  memberId: string;
  accountId: string;
  email: string;
  displayName: string;
  emailVerified: boolean;
  secondFactor: boolean;
  recoveryCodesLeft: number;
  /** One of the venue's own people, which decides only which environments the bootstrap names to them. */
  staff: boolean;
};

/** One of a member's sessions, as `GET /v4/sessions` lists them, so they can end one they do not recognise. */
export type HeldSession = {
  sessionId: string;
  device: string | null;
  createdAt: string;
  lastUsedAt: string | null;
  expiresAt: string;
  endedAt: string | null;
  live: boolean;
};

/** A second factor being set up: what goes into an authenticator app. */
export type TotpStart = { secret: string; uri: string; digits: number; periodSeconds: number };

/** Signing up, asking for a code again, and asking for a new password all answer this and nothing else — the same
 * whether or not the address is anybody's (interface 1 README, members and sessions). */
export type CheckYourEmail = { checkYourEmail: true };

/** What an order asks for. The tick comes from the market's ladder, so the app never invents a price. */
export type OrderRequest = {
  clientOrderId: string;
  instrumentId: string;
  action: Side;
  outcome: Outcome;
  price: { tick: number };
  quantity: number;
  /** Left out, the order rests until it is cancelled. `IOC` is what taking a quote wants: fill against what is
   * there now and cancel the rest, rather than leaving a resting order behind when the quote has already gone. */
  timeInForce?: "GTC" | "IOC" | "FOK";
};

/** One leg of a parlay, as the app asks for it: a market and which way it has to go. */
export type ParlayLegInput = { instrumentId: string; outcome: Outcome };

/** One leg of a parlay, as the venue names it back, with the words a person reads. */
export type ParlayLeg = {
  eventId: string;
  instrumentId: string;
  outcome: Outcome;
  /** The event's title, e.g. "Who wins the final?". */
  title: string;
  /** The market's label within that event, e.g. "Team A". */
  label: string;
};

/** A request for quote: legs, how many contracts, and how long anyone has to answer it.
 *
 * `instrumentId` is the parlay the venue listed for these legs, which is a market like any other — the quotes are
 * real sell orders resting on its book, and taking one is an ordinary buy. */
export type Rfq = {
  requestId: string;
  instrumentId: string;
  eventId: string;
  legs: ParlayLeg[];
  quantity: number;
  /** Contracts trade in multiples of this, and a request is for at least one multiple. */
  lotStep: number;
  openedAt: string;
  expiresAt: string;
  /** Only on the answer to making a request: whether this request is what listed the parlay. */
  listed?: boolean;
  /** Only on the answer to making a request: how many responders it reached. Zero means nobody was listening, so
   * no quote is coming however long the window runs. */
  responders?: number;
};

/** A firm quote: a sell order resting on the parlay's book, which the requester may take at that price. */
export type Quote = {
  requestId: string;
  orderId: string;
  instrumentId: string;
  price: Price;
  /** Contracts still available. Falls as it is filled. */
  quantity: number;
  /** Contracts already bought, by anyone. */
  filled: number;
  /** When the quote lapses. Null means it rests until the responder cancels it. */
  expiresAt?: string | null;
  state: "open" | "filled" | "expired" | "cancelled";
};

/** A request with the quotes that have come in for it, from GET /v4/rfqs/{requestId}. */
export type RfqStatus = Rfq & {
  listed: boolean;
  /** Whether the window is still running. Once false no new quote will arrive. */
  open: boolean;
  quotes: Quote[];
};

/** What the venue answered an order with. A rejected order is an answer, not an error: the venue took the request
 * and the engine said no, and `rejectReason` says why. */
export type OrderAnswer = {
  status: string;
  orderId: string | null;
  rejectReason?: string | null;
  sequence: string;
  executions: Execution[];
};

export type Execution = {
  type: string;
  orderId: string | null;
  instrumentId: string;
  clientOrderId?: string;
  price?: Price;
  quantity?: number;
  remaining?: number;
  reason?: string;
  sequence: string;
};

/** A refusal from the venue: its code is what to act on, its message is what to show. */
export class VenueError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly status: number,
    /** Seconds to wait, when a flood limit said so. */
    readonly retryAfter?: number,
  ) {
    super(message);
    this.name = "VenueError";
  }

  /** Whether trying the same thing later could work: a flood limit, or something the venue could not reach. */
  get worthRetrying(): boolean {
    return this.status === 429 || this.status === 503;
  }
}
