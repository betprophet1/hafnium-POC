// The venue client and the private stream, tested where no phone is needed: Node, with a fetch and a socket that
// answer what the venue would. What is tested here is what the app would otherwise get wrong on three platforms at
// once — how a refusal reaches it, and what a stream does when a connection comes and goes.

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { Venue, marketsOf } from "./client";
import {
  byEvent,
  filledContracts,
  legsOk,
  money,
  offerable,
  payoutCents,
  quantityOk,
  secondsUntil,
  stillTrading,
} from "./parlay";
import { BUILT_IN, fallback, readKept, stillOffered, withBuiltIn } from "./environment";
import { openPrivateStream, higher } from "./stream";
import type { Timers, WebSocketLike } from "./stream";
import { VenueError } from "./types";

/** A fetch that answers from a list of canned replies, and records what it was asked. */
function fetchStub(replies: { status: number; body: unknown; headers?: Record<string, string> }[]) {
  const asked: { url: string; method: string; body?: unknown; auth?: string }[] = [];
  const doFetch = async (url: unknown, init?: unknown): Promise<Response> => {
    const options = (init ?? {}) as { method?: string; body?: string; headers?: Record<string, string> };
    asked.push({
      url: String(url),
      method: options.method ?? "GET",
      body: options.body ? JSON.parse(options.body) : undefined,
      auth: options.headers?.Authorization,
    });
    const reply = replies.shift() ?? { status: 500, body: null };
    return new Response(reply.body === null ? "" : JSON.stringify(reply.body), {
      status: reply.status,
      headers: reply.headers,
    });
  };
  return { doFetch: doFetch as unknown as typeof fetch, asked };
}

describe("the venue client", () => {
  const member = {
    memberId: "m_0123456789abcdef",
    accountId: "100000",
    email: "someone@example.com",
    displayName: "someone",
    emailVerified: true,
    session: { sessionId: "vs_0123456789abcdef", credential: "vs_0123456789abcdef.aa", expiresAt: "2026-10-18" },
  };

  it("signs a member up, proves the address, logs in, and then speaks as them", async () => {
    const { doFetch, asked } = fetchStub([
      { status: 200, body: { checkYourEmail: true } },
      { status: 200, body: { verified: true } },
      { status: 200, body: member },
      { status: 200, body: { accountId: "100000", availableCents: 0, reservedCents: 0, positions: [], orders: [] } },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test/", fetch: doFetch });

    // Signing up answers only that a message went out: there is no session here to hold.
    const asking = await venue.signUp("someone@example.com", "a good long password");
    assert.deepEqual(asking, { checkYourEmail: true });
    await venue.verifyEmail("0123456789abcdef0123456789abcdef");
    const signed = await venue.logIn("someone@example.com", "a good long password", undefined, "an iPhone");
    venue.credential = signed.session.credential;
    await venue.account();

    assert.equal(asked[0].url, "https://venue.test/v4/members");
    assert.equal(asked[0].auth, undefined, "signing up is not signed in");
    assert.deepEqual(asked[0].body, { email: "someone@example.com", password: "a good long password" });
    assert.equal(asked[1].url, "https://venue.test/v4/members/verify");
    assert.equal(asked[1].auth, undefined, "proving an address is reached with no session, which is the point");
    assert.deepEqual(asked[2].body, {
      email: "someone@example.com",
      password: "a good long password",
      device: "an iPhone",
    });
    assert.equal(asked[3].auth, "Bearer vs_0123456789abcdef.aa", "afterwards every call carries the session");
  });

  it("asks for a second-factor code rather than saying the password was wrong", async () => {
    const { doFetch, asked } = fetchStub([
      {
        status: 401,
        body: { error: { code: "SECOND_FACTOR_REQUIRED", message: "this account asks for a code" } },
      },
      { status: 200, body: member },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test", fetch: doFetch });

    // The one refusal that means "send something different", not "send that again".
    const wanted = await venue.logIn("someone@example.com", "a good long password").catch((e: unknown) => e);
    assert.ok(wanted instanceof VenueError);
    assert.equal(wanted.code, "SECOND_FACTOR_REQUIRED");
    assert.equal(wanted.worthRetrying, false, "retrying the same request would be refused the same way");

    await venue.logIn("someone@example.com", "a good long password", "123456");
    assert.equal((asked[1].body as { code?: string }).code, "123456");
  });

  it("locks a member out with a code of its own, so an app can say what is actually wrong", async () => {
    const { doFetch } = fetchStub([
      { status: 429, body: { error: { code: "LOCKED_OUT", message: "too many attempts" } } },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test", fetch: doFetch });
    const locked = await venue.logIn("someone@example.com", "a good long password").catch((e: unknown) => e);
    assert.ok(locked instanceof VenueError);
    assert.equal(locked.code, "LOCKED_OUT");
    assert.equal(locked.status, 429);
  });

  it("reaches a member's own routes with their session and nothing else", async () => {
    const { doFetch, asked } = fetchStub([
      {
        status: 200,
        body: { ...member, secondFactor: true, recoveryCodesLeft: 9 },
      },
      {
        status: 200,
        body: {
          sessions: [
            { sessionId: "vs_0123456789abcdef", device: "an iPhone", createdAt: "2026-09-18", live: true },
            { sessionId: "vs_fedcba9876543210", device: "a browser", createdAt: "2026-09-17", live: true },
          ],
        },
      },
      { status: 200, body: {} },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test", credential: "vs_0123456789abcdef.aa", fetch: doFetch });

    const about = await venue.about();
    assert.equal(about.secondFactor, true);
    assert.equal(about.recoveryCodesLeft, 9);
    const held = await venue.sessions();
    assert.equal(held.length, 2);
    await venue.endSession("vs_fedcba9876543210");

    assert.equal(asked[0].url, "https://venue.test/v4/members/me");
    assert.equal(asked[1].url, "https://venue.test/v4/sessions");
    // The member is never named in the request: the session says who is asking, so it can only reach its own.
    assert.equal(asked[2].url, "https://venue.test/v4/sessions/vs_fedcba9876543210");
    assert.equal(asked[2].method, "DELETE");
    for (const call of asked) assert.equal(call.auth, "Bearer vs_0123456789abcdef.aa");
  });

  it("turns a refusal into something the app can act on", async () => {
    const { doFetch } = fetchStub([
      { status: 429, body: { error: { code: "RATE_LIMITED", message: "too many" } }, headers: { "Retry-After": "3" } },
      { status: 401, body: { error: { code: "UNAUTHORIZED", message: "the session has expired" } } },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test", credential: "vs_0123456789abcdef.aa", fetch: doFetch });

    const limited = await venue.account().catch((e: unknown) => e);
    assert.ok(limited instanceof VenueError);
    assert.equal(limited.code, "RATE_LIMITED");
    assert.equal(limited.retryAfter, 3);
    assert.equal(limited.worthRetrying, true);

    const expired = await venue.account().catch((e: unknown) => e);
    assert.ok(expired instanceof VenueError);
    assert.equal(expired.worthRetrying, false, "an expired session is not mended by trying again");
  });

  it("will not send a signed-in call without a session, and says so before asking the network", async () => {
    const { doFetch, asked } = fetchStub([]);
    const venue = new Venue({ baseUrl: "https://venue.test", fetch: doFetch });
    const refused = await venue.account().catch((e: unknown) => e);
    assert.ok(refused instanceof VenueError);
    assert.equal(refused.status, 401);
    assert.equal(asked.length, 0, "nothing was sent");
  });

  it("says the venue is unreachable rather than throwing whatever the platform threw", async () => {
    const venue = new Venue({
      baseUrl: "https://venue.test",
      credential: "vs_0123456789abcdef.aa",
      fetch: (async () => {
        throw new TypeError("Network request failed");
      }) as unknown as typeof fetch,
    });
    const failed = await venue.account().catch((e: unknown) => e);
    assert.ok(failed instanceof VenueError);
    assert.equal(failed.code, "UNREACHABLE");
    assert.equal(failed.worthRetrying, true);
  });

  it("flattens events into the markets a person picks from", () => {
    const markets = marketsOf([
      {
        eventId: "1000001",
        title: "Will it rain in London tomorrow?",
        markets: [{ instrumentId: "100000100", label: "Rain", class: "binary", state: "OPEN" }],
      },
    ]);
    assert.deepEqual(markets, [
      {
        instrumentId: "100000100",
        eventId: "1000001",
        label: "Rain",
        title: "Will it rain in London tomorrow?",
        state: "OPEN",
        class: "binary",
        outcome: null,
      },
    ]);
  });

  // A venue that does not send `class` is older than parlays, so nothing it lists is one. Calling such a market
  // binary keeps it offerable as a leg; the alternative, guessing "parlay", would hide every market on that venue.
  it("calls a market binary when the venue did not say what kind it is", () => {
    const markets = marketsOf([
      {
        eventId: "1000001",
        title: "Will it rain in London tomorrow?",
        markets: [{ instrumentId: "100000100", label: "Rain", state: "OPEN" }],
      },
    ]);
    assert.equal(markets[0].class, "binary");
  });

  it("asks for quotes on a parlay, then reads what came back for it", async () => {
    const rfq = {
      requestId: "r7x",
      instrumentId: "4503599627370496",
      eventId: "4503599627370496",
      legs: [
        { eventId: "1000001", instrumentId: "100000100", outcome: "yes", title: "Rain?", label: "Rain" },
        { eventId: "1000002", instrumentId: "100000200", outcome: "no", title: "Snow?", label: "Snow" },
      ],
      quantity: 20,
      lotStep: 10,
      openedAt: "2026-09-21T12:00:00Z",
      expiresAt: "2026-09-21T12:00:30Z",
      listed: true,
      responders: 2,
    };
    const quote = {
      requestId: "r7x",
      orderId: "900001",
      instrumentId: "4503599627370496",
      price: { tick: 40, cents: "4", probability: "0.0400", american: "+2400" },
      quantity: 20,
      filled: 0,
      expiresAt: "2026-09-21T12:00:25Z",
      state: "open",
    };
    const { doFetch, asked } = fetchStub([
      { status: 200, body: rfq },
      { status: 200, body: { ...rfq, open: true, quotes: [quote] } },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test", credential: "vs_x.aa", fetch: doFetch });

    const opened = await venue.requestQuote(
      [
        { instrumentId: "100000100", outcome: "yes" },
        { instrumentId: "100000200", outcome: "no" },
      ],
      20,
    );
    assert.equal(opened.requestId, "r7x");
    assert.equal(opened.responders, 2);

    const status = await venue.rfq(opened.requestId);
    assert.equal(status.open, true);
    assert.equal(status.quotes[0].price.cents, "4");

    // The legs go as the venue words them — `outcome`, not a boolean — and the request is named by the path, never
    // in the body, which the gateway refuses.
    assert.equal(asked[0].method, "POST");
    assert.equal(asked[0].url, "https://venue.test/v4/rfqs");
    assert.deepEqual(asked[0].body, {
      legs: [
        { instrumentId: "100000100", outcome: "yes" },
        { instrumentId: "100000200", outcome: "no" },
      ],
      quantity: 20,
    });
    assert.equal(asked[1].method, "GET");
    assert.equal(asked[1].url, "https://venue.test/v4/rfqs/r7x");
    assert.equal(asked[1].auth, "Bearer vs_x.aa");
  });

  it("gives back the venue's own words when a request for quote is refused", async () => {
    const { doFetch } = fetchStub([
      {
        status: 400,
        body: { error: { code: "BAD_REQUEST", message: "a request is for at least 10 contracts" } },
      },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test", credential: "vs_x.aa", fetch: doFetch });
    await assert.rejects(
      () => venue.requestQuote([{ instrumentId: "100000100", outcome: "yes" }], 1),
      (e: unknown) => {
        assert.ok(e instanceof VenueError);
        assert.equal(e.code, "BAD_REQUEST");
        assert.equal(e.message, "a request is for at least 10 contracts");
        return true;
      },
    );
  });

  // Taking a quote is an ordinary buy, and IOC is the part that matters: a resting order left on a parlay nobody is
  // quoting any more is not what somebody who pressed "buy at 4¢" asked for.
  it("takes a quote as an immediate-or-cancel buy of YES at the quote's tick", async () => {
    const { doFetch, asked } = fetchStub([
      { status: 200, body: { status: "filled", orderId: "900002", sequence: "42", executions: [] } },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test", credential: "vs_x.aa", fetch: doFetch });
    await venue.placeOrder({
      clientOrderId: "parlay-abc",
      instrumentId: "4503599627370496",
      action: "buy",
      outcome: "yes",
      price: { tick: 40 },
      quantity: 20,
      timeInForce: "IOC",
    });
    assert.equal(asked[0].url, "https://venue.test/v4/orders");
    assert.deepEqual(asked[0].body, {
      clientOrderId: "parlay-abc",
      instrumentId: "4503599627370496",
      action: "buy",
      outcome: "yes",
      price: { tick: 40 },
      quantity: 20,
      timeInForce: "IOC",
    });
  });
});

/** A socket a test drives by hand. */
class FakeSocket implements WebSocketLike {
  sent: string[] = [];
  closed = false;
  onopen: ((event?: unknown) => void) | null = null;
  onclose: ((event?: unknown) => void) | null = null;
  onerror: ((event?: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;

  send(data: string) {
    this.sent.push(data);
  }

  close() {
    this.closed = true;
    this.onclose?.();
  }

  say(message: unknown) {
    this.onmessage?.({ data: JSON.stringify(message) });
  }

  /** What the client sent, as the objects it sent. */
  get ops(): Record<string, unknown>[] {
    return this.sent.map((s) => JSON.parse(s) as Record<string, unknown>);
  }
}

/** Timers a test runs by hand, so nothing waits. */
function fakeTimers() {
  const due: (() => void)[] = [];
  const timers: Timers = {
    setTimeout: (run) => {
      due.push(run);
      return due.length;
    },
    clearTimeout: () => {},
    setInterval: () => 0,
    clearInterval: () => {},
  };
  return { timers, run: () => due.splice(0).forEach((run) => run()) };
}

describe("choosing an environment", () => {
  const prod = { id: "prod", label: "Live", url: "https://venue.test", live: true };
  const preview = { id: "pr-7", label: "PR 7", url: "https://pr-7.venue.test", live: false };

  it("asks the venue where it may go, and carries the session only when there is one", async () => {
    const { doFetch, asked } = fetchStub([
      { status: 200, body: { environments: [prod] } },
      { status: 200, body: { environments: [prod, preview] } },
    ]);

    // Before anyone has signed in: asked without a credential, and the venue names only the live one.
    const anonymous = new Venue({ baseUrl: "https://venue.test", fetch: doFetch });
    assert.deepEqual(await anonymous.bootstrap(), [prod]);
    assert.equal(asked[0].url, "https://venue.test/v4/bootstrap");
    assert.equal(asked[0].auth, undefined, "a stranger asks as a stranger");

    // With a session, the venue may name more. Whether it does is the venue's business, not the app's.
    const signed = new Venue({ baseUrl: "https://venue.test", credential: "vs_0123456789abcdef.aa", fetch: doFetch });
    assert.deepEqual(await signed.bootstrap(), [prod, preview]);
    assert.equal(asked[1].auth, "Bearer vs_0123456789abcdef.aa");
  });

  it("keeps a session with the venue it belongs to, and refuses to read one that does not say", () => {
    const kept = { environment: preview, member: { session: { credential: "vs_0123456789abcdef.aa" } } };
    assert.deepEqual(readKept(JSON.stringify(kept)), kept);

    // Anything that does not name a venue is treated as nothing kept at all. That costs a sign-in; the alternative
    // is replaying a credential against a venue it was never issued for, which is the one thing that must not happen.
    assert.equal(readKept(null), null);
    assert.equal(readKept("not json"), null);
    assert.equal(readKept(JSON.stringify({ member: { session: {} } })), null, "no venue named");
    assert.equal(readKept(JSON.stringify({ environment: preview })), null, "no member");
    assert.equal(readKept(JSON.stringify({ environment: { id: "x" }, member: {} })), null, "no url");
  });

  it("goes back to the live venue when the one a session belongs to has gone", () => {
    const kept = { environment: preview, member: {} };
    assert.equal(stillOffered(kept, [prod, preview]), true);
    // A preview environment outlives no pull request: once it is reaped it is simply not on the list any more.
    assert.equal(stillOffered(kept, [prod]), false);
    // And one that kept the name but moved is not the same venue either.
    assert.equal(stillOffered(kept, [prod, { ...preview, url: "https://pr-7b.venue.test" }]), false);
  });

  it("always offers the way home, and never twice", () => {
    // A preview names only itself, so without this a member who moved to one would be shown a list of one and have
    // no way back but signing out. The built-in address is the only venue the app knows without being told.
    assert.deepEqual(withBuiltIn([preview]), [preview, BUILT_IN]);
    // The live venue is normally that same address under a better name; offering it twice would only raise the
    // question of which one is which.
    assert.deepEqual(withBuiltIn([{ ...prod, url: BUILT_IN.url }]), [{ ...prod, url: BUILT_IN.url }]);
    assert.deepEqual(withBuiltIn([{ ...prod, url: `${BUILT_IN.url}/` }]), [{ ...prod, url: `${BUILT_IN.url}/` }]);
    // Told nothing at all, an app still knows where it was built pointing.
    assert.deepEqual(withBuiltIn([]), [BUILT_IN]);
  });

  it("always has somewhere to point", () => {
    assert.equal(fallback([prod, preview]), prod, "the live one");
    assert.equal(fallback([preview]), preview, "or whatever there is");
    assert.equal(fallback([]), BUILT_IN, "or the address it was built with");
  });
});

describe("the private stream", () => {
  it("logs in, and asks to carry on from where it was when the connection comes back", () => {
    const sockets: FakeSocket[] = [];
    const { timers, run } = fakeTimers();
    const seen: string[] = [];
    const opens: boolean[] = [];
    const stream = openPrivateStream({
      url: "wss://venue.test/v4/ws_private",
      credential: "vs_0123456789abcdef.aa",
      timers,
      backoff: () => 0,
      socket: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
      handlers: {
        onMessage: (m) => seen.push(m.type),
        onOpen: (w) => opens.push(w.resumed),
      },
    });

    sockets[0].onopen?.();
    assert.deepEqual(sockets[0].ops[0], { op: "login", id: "login", token: "vs_0123456789abcdef.aa" });
    sockets[0].say({ type: "welcome", sessionId: "7", resumed: false });
    sockets[0].say({ type: "execution", sequence: "41" });
    sockets[0].say({ type: "account", sequence: "42" });
    assert.deepEqual(seen, ["execution", "account"]);
    assert.equal(stream.seen(), "42");

    // The connection drops. The next one asks for everything after what it had.
    sockets[0].onclose?.();
    run();
    assert.equal(sockets.length, 2, "it connected again");
    sockets[1].onopen?.();
    assert.deepEqual(sockets[1].ops[0], {
      op: "login",
      id: "login",
      token: "vs_0123456789abcdef.aa",
      resumeFrom: "42",
    });

    // The venue cannot carry on from there: the app is told, so it reads the account again.
    sockets[1].say({ type: "resync", sequence: "80" });
    assert.deepEqual(opens, [false, false]);

    stream.stop();
    assert.ok(sockets[1].closed);
    sockets[1].onclose?.();
    run();
    assert.equal(sockets.length, 2, "a stopped stream does not connect again");
  });

  it("passes a refusal on rather than swallowing it", () => {
    const { timers } = fakeTimers();
    const socket = new FakeSocket();
    const errors: string[] = [];
    openPrivateStream({
      url: "wss://venue.test/v4/ws_private",
      credential: "vs_0123456789abcdef.aa",
      timers,
      socket: () => socket,
      handlers: { onMessage: () => {}, onError: (e) => errors.push(e.code) },
    });
    socket.onopen?.();
    socket.say({ type: "error", error: { code: "UNAUTHORIZED", message: "a valid token is required" } });
    assert.deepEqual(errors, ["UNAUTHORIZED"]);
  });

  it("compares sequences as the numbers they are, not as text", () => {
    assert.equal(higher(undefined, "9"), "9");
    assert.equal(higher("9", "10"), "10", "ten comes after nine");
    assert.equal(higher("10", "9"), "10");
    assert.equal(higher("100000000000000002", "100000000000000010"), "100000000000000010");
  });
});

describe("building a parlay", () => {
  const market = (eventId: string, instrumentId: string, label: string, kind = "binary", state = "open") => ({
    instrumentId,
    eventId,
    label,
    title: `Event ${eventId}`,
    state,
    class: kind,
    outcome: null,
  });

  it("offers only markets that are trading, and never a parlay as a leg of another", () => {
    const markets = [
      market("1", "101", "Rain"),
      market("1", "102", "No rain", "binary", "halted"),
      market("2", "201", "Arsenal"),
      // The venue lists one of these for every request; picking it as a leg is refused, so it is not shown.
      market("9", "901", "Parlay: Rain and Arsenal", "parlay"),
    ];
    assert.deepEqual(
      offerable(markets).map((m) => m.instrumentId),
      ["101", "201"],
    );
  });

  it("groups the markets under their event, which is what stops two legs on one", () => {
    const grouped = byEvent([market("1", "101", "Rain"), market("2", "201", "Arsenal"), market("1", "102", "Snow")]);
    assert.deepEqual(
      grouped.map((g) => [g.eventId, g.markets.length]),
      [
        ["1", 2],
        ["2", 1],
      ],
    );
  });

  it("knows which leg counts and quantities the venue would take", () => {
    assert.equal(legsOk(1), false, "a parlay of one is just a market");
    assert.equal(legsOk(2), true);
    assert.equal(legsOk(6), true);
    assert.equal(legsOk(7), false);
    assert.equal(quantityOk(10), true);
    assert.equal(quantityOk(20), true);
    assert.equal(quantityOk(15), false, "off the lot step");
    assert.equal(quantityOk(0), false);
    assert.equal(quantityOk(1.5), false);
    assert.equal(quantityOk(Number.NaN), false, "an empty box is not a quantity");
  });

  // The wire carries an `accepted` execution beside each `fill`, and an order that filled completely still answers
  // `accepted` rather than `filled`. Counting anything but the fills gets this wrong in both directions.
  it("counts contracts from the fills, not from the status or every execution", () => {
    const answer = {
      status: "accepted",
      orderId: "900002",
      sequence: "34",
      executions: [
        { type: "accepted", orderId: "900002", instrumentId: "45", quantity: 20, remaining: 20, sequence: "34" },
        { type: "fill", orderId: "900002", instrumentId: "45", quantity: 20, remaining: 0, sequence: "34" },
      ],
    };
    assert.equal(filledContracts(answer), 20);
    assert.equal(filledContracts({ ...answer, executions: [] }), 0, "the quote had already gone");
  });

  // A contract pays a dollar. The price paid for it — fractions of a cent, for a long parlay — is not the payout,
  // and saying so would be wrong by orders of magnitude.
  it("pays a dollar a contract, whatever the contract cost", () => {
    assert.equal(payoutCents(20), 2000);
    assert.equal(money(payoutCents(20)), "$20.00");
    assert.equal(money(payoutCents(1)), "$1.00");
  });

  it("counts a window down, and says nothing is left rather than a negative number", () => {
    const now = Date.parse("2026-09-21T12:00:00Z");
    assert.equal(secondsUntil("2026-09-21T12:00:30Z", now), 30);
    assert.equal(secondsUntil("2026-09-21T11:59:30Z", now), 0, "already over");
    assert.equal(secondsUntil(null, now), 0, "a quote that rests until cancelled");
    assert.equal(secondsUntil("not a time", now), 0);
  });
});

describe("a ticket that outlives its round", () => {
  const market = (eventId: string, instrumentId: string, label: string, kind = "binary", state = "open") => ({
    instrumentId,
    eventId,
    label,
    title: `Event ${eventId}`,
    state,
    class: kind,
    outcome: null,
  });
  const pick = (instrumentId: string) => ({
    instrumentId,
    outcome: "yes" as const,
    title: "Event",
    label: "Label",
  });

  // This is what actually went wrong in use: rounds roll every couple of minutes, the screen held the list it had
  // loaded on open, and the request went out naming a market that had closed. The venue takes it and no responder
  // can price it, so it reads as a quote that never comes rather than as a mistake.
  it("drops a leg whose market has stopped trading", () => {
    const picks = new Map([
      ["1", pick("101")],
      ["2", pick("201")],
    ]);
    const now = [market("1", "101", "Rain"), market("2", "201", "Arsenal", "binary", "closed")];
    const kept = stillTrading(picks, now);
    assert.deepEqual([...kept.keys()], ["1"]);
  });

  it("drops a leg whose market has gone from the list entirely, as a finished round does", () => {
    const picks = new Map([["2", pick("201")]]);
    assert.equal(stillTrading(picks, [market("1", "101", "Rain")]).size, 0);
  });

  it("keeps every leg while the round is still trading", () => {
    const picks = new Map([
      ["1", pick("101")],
      ["2", pick("201")],
    ]);
    const now = [market("1", "101", "Rain"), market("2", "201", "Arsenal")];
    assert.equal(stillTrading(picks, now).size, 2);
  });
});
