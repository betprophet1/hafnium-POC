// The venue client and the private stream, tested where no phone is needed: Node, with a fetch and a socket that
// answer what the venue would. What is tested here is what the app would otherwise get wrong on three platforms at
// once — how a refusal reaches it, and what a stream does when a connection comes and goes.

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { Venue, marketsOf } from "./client";
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
  it("signs a member in and then speaks as them", async () => {
    const member = {
      memberId: "m_0123456789abcdef",
      accountId: "100000",
      email: "someone@example.com",
      displayName: "someone",
      session: { sessionId: "vs_0123456789abcdef", credential: "vs_0123456789abcdef.aa", expiresAt: "2026-10-18" },
    };
    const { doFetch, asked } = fetchStub([
      { status: 200, body: member },
      { status: 200, body: { accountId: "100000", availableCents: 0, reservedCents: 0, positions: [], orders: [] } },
    ]);
    const venue = new Venue({ baseUrl: "https://venue.test/", fetch: doFetch });
    const signed = await venue.signUp("someone@example.com", "a good long password");
    venue.credential = signed.session.credential;
    await venue.account();

    assert.equal(asked[0].url, "https://venue.test/v4/members");
    assert.equal(asked[0].auth, undefined, "signing up is not signed in");
    assert.deepEqual(asked[0].body, { email: "someone@example.com", password: "a good long password" });
    assert.equal(asked[1].auth, "Bearer vs_0123456789abcdef.aa", "afterwards every call carries the session");
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
        markets: [{ instrumentId: "100000100", label: "Rain", state: "OPEN" }],
      },
    ]);
    assert.deepEqual(markets, [
      {
        instrumentId: "100000100",
        eventId: "1000001",
        label: "Rain",
        title: "Will it rain in London tomorrow?",
        state: "OPEN",
        outcome: null,
      },
    ]);
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
