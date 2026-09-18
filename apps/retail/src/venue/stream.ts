// The private stream: what the venue tells one account as things happen to it (interface 1 README, sessions).
//
// Three things make this more than a socket. It logs in with the session credential before anything else. It
// remembers the highest sequence it has seen and asks to resume from it when the connection comes back, so a phone
// that went through a tunnel is told what it missed rather than starting blank. And when the venue answers that it
// cannot resume — `resync` — it says so, because then the app must read the account again rather than believe what
// it holds.
//
// Nothing here knows about React: it takes callbacks and gives back a stop function.

export type StreamMessage = {
  type: string;
  sequence?: string;
  [field: string]: unknown;
};

export type StreamHandlers = {
  /** Every message the venue sends, after `welcome`. */
  onMessage: (message: StreamMessage) => void;
  /** The session is live. `resumed` is false when the venue could not carry on from where this app was, and the
   * app should read the account again. */
  onOpen?: (welcome: { resumed: boolean; sessionId?: string }) => void;
  onClosed?: () => void;
  /** Something the venue refused: a login that failed, an op it would not take. */
  onError?: (error: { code: string; message: string }) => void;
};

export type StreamOptions = {
  url: string;
  credential: string;
  handlers: StreamHandlers;
  /** For tests, and for React Native, whose WebSocket is not the browser's. */
  socket?: (url: string) => WebSocketLike;
  /** How long to wait before connecting again, in milliseconds, given how many attempts have failed. */
  backoff?: (attempt: number) => number;
  /** For tests: the timers to use. */
  timers?: Timers;
};

/** Only the parts of a WebSocket this uses, so a test can stand in for one. */
export interface WebSocketLike {
  send(data: string): void;
  close(): void;
  onopen: ((event?: unknown) => void) | null;
  onclose: ((event?: unknown) => void) | null;
  onerror: ((event?: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
}

export type Timers = {
  setTimeout: (run: () => void, ms: number) => unknown;
  clearTimeout: (handle: unknown) => void;
  setInterval: (run: () => void, ms: number) => unknown;
  clearInterval: (handle: unknown) => void;
};

/** The venue closes a session it has not heard from; every client sends this. */
const HEARTBEAT_MS = 15_000;

/** Waits between attempts: a second, then longer, never more than half a minute. */
function defaultBackoff(attempt: number): number {
  return Math.min(1000 * 2 ** Math.max(0, attempt - 1), 30_000);
}

export type Stream = {
  /** Stops for good: no more messages, no more reconnecting. */
  stop: () => void;
  /** The highest sequence seen, which is what a resume asks from. */
  seen: () => string | undefined;
};

export function openPrivateStream(options: StreamOptions): Stream {
  const timers: Timers = options.timers ?? {
    setTimeout: (run, ms) => globalThis.setTimeout(run, ms),
    clearTimeout: (handle) => globalThis.clearTimeout(handle as never),
    setInterval: (run, ms) => globalThis.setInterval(run, ms),
    clearInterval: (handle) => globalThis.clearInterval(handle as never),
  };
  const makeSocket = options.socket ?? ((url: string) => new globalThis.WebSocket(url) as unknown as WebSocketLike);
  const backoff = options.backoff ?? defaultBackoff;

  let socket: WebSocketLike | null = null;
  let beat: unknown = null;
  let retry: unknown = null;
  let attempt = 0;
  let stopped = false;
  let seen: string | undefined;

  const stopBeating = () => {
    if (beat !== null) timers.clearInterval(beat);
    beat = null;
  };

  const connect = () => {
    if (stopped) return;
    const ws = makeSocket(options.url);
    socket = ws;
    ws.onopen = () => {
      attempt = 0;
      const login: Record<string, unknown> = { op: "login", id: "login", token: options.credential };
      // Everything since this, please: a phone that lost its connection is told what happened while it was away.
      if (seen !== undefined) login.resumeFrom = seen;
      ws.send(JSON.stringify(login));
      beat = timers.setInterval(() => ws.send(JSON.stringify({ op: "heartbeat", id: "heartbeat" })), HEARTBEAT_MS);
    };
    ws.onmessage = (event) => {
      let message: StreamMessage;
      try {
        message = JSON.parse(String(event.data)) as StreamMessage;
      } catch {
        return;
      }
      if (typeof message.sequence === "string") seen = higher(seen, message.sequence);
      switch (message.type) {
        case "welcome":
          options.handlers.onOpen?.({
            resumed: message.resumed === true,
            sessionId: typeof message.sessionId === "string" ? message.sessionId : undefined,
          });
          break;
        case "resync":
          // The venue cannot carry on from where this app was: what it holds is not to be trusted.
          options.handlers.onOpen?.({ resumed: false });
          break;
        case "error": {
          const error = message.error as { code?: string; message?: string } | undefined;
          options.handlers.onError?.({ code: error?.code ?? "ERROR", message: error?.message ?? "refused" });
          break;
        }
        default:
          options.handlers.onMessage(message);
      }
    };
    ws.onerror = () => {
      // A failure is followed by a close, which is where reconnecting happens.
    };
    ws.onclose = () => {
      stopBeating();
      socket = null;
      options.handlers.onClosed?.();
      if (stopped) return;
      attempt += 1;
      retry = timers.setTimeout(connect, backoff(attempt));
    };
  };

  connect();

  return {
    stop: () => {
      stopped = true;
      stopBeating();
      if (retry !== null) timers.clearTimeout(retry);
      retry = null;
      socket?.close();
      socket = null;
    },
    seen: () => seen,
  };
}

/** The higher of two sequences. They are 64-bit numbers as text, so they are compared by length and then digits,
 * never by turning them into JavaScript numbers, which cannot hold them. */
export function higher(a: string | undefined, b: string): string {
  if (a === undefined) return b;
  if (a.length !== b.length) return a.length > b.length ? a : b;
  return a >= b ? a : b;
}
