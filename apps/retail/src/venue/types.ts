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
