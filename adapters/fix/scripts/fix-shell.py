#!/usr/bin/env python3
"""A FIX 4.4 console for trying the venue's FIX adapter by hand, with nothing to install but Python 3.

It logs on with a dev token (interfaces/fix/README.md), keeps the session alive, prints every report the venue sends
as it arrives, and turns short commands into orders. It is a test tool, not a FIX engine: it does not store sequence
numbers (every logon resets them with 141=Y) and does not answer resend requests.

    python3 fix-shell.py --host 127.0.0.1 --port 17420 --token dev-retail-customer

Commands (type `help`):
    list                                  markets: instrument ID, open or not, lot step, name
    buy SYMBOL QTY PRICE [gtc|ioc|fok] [post]
    sell SYMBOL QTY PRICE [gtc|ioc|fok] [post]
    replace CLORDID QTY PRICE             new total size (filled part included) and price
    cancel CLORDID
    cancelall [SYMBOL]                    every resting order of the account, or those in one market
    raw 35=D|55=...|54=1|...              any message, fields separated by |
    sleep SECONDS                         wait, printing what arrives (for scripted runs from stdin)
    quit                                  log out

Prices are YES cents on the market's ladder (`53`, `0.5`); side buy/sell is YES (README, sides, outcomes and prices).
"""
import argparse
import socket
import sys
import threading
import time

SOH = "\x01"
NAMES = {
    "0": "Heartbeat", "1": "TestRequest", "2": "ResendRequest", "3": "Reject", "4": "SequenceReset", "5": "Logout",
    "8": "ExecutionReport", "9": "OrderCancelReject", "A": "Logon", "j": "BusinessMessageReject",
    "r": "OrderMassCancelReport", "y": "SecurityList",
}
EXEC_TYPES = {"0": "new", "F": "fill", "5": "replaced", "4": "cancelled", "D": "restated", "8": "rejected"}
TIF = {"gtc": "1", "ioc": "3", "fok": "4"}


def now():
    return time.strftime("%Y%m%d-%H:%M:%S.000", time.gmtime())


class Session:
    def __init__(self, host, port, sender, target, token, heartbeat):
        self.sock = socket.create_connection((host, port), timeout=10)
        self.sock.settimeout(None)
        self.sender, self.target, self.token, self.heartbeat = sender, target, token, heartbeat
        self.seq = 0
        self.lock = threading.Lock()
        self.last_sent = time.monotonic()
        self.orders = {}  # ClOrdID -> (symbol, side), for cancel and replace
        self.next_id = 0
        self.open = True
        self.logged_on = threading.Event()

    def send(self, msg_type, fields):
        with self.lock:
            self.seq += 1
            body = [("35", msg_type), ("49", self.sender), ("56", self.target), ("34", str(self.seq)), ("52", now())]
            text = "".join(f"{k}={v}{SOH}" for k, v in body + fields)
            head = f"8=FIX.4.4{SOH}9={len(text.encode())}{SOH}"
            checksum = sum((head + text).encode()) % 256
            self.sock.sendall((head + text + f"10={checksum:03d}{SOH}").encode())
            self.last_sent = time.monotonic()

    def clordid(self):
        self.next_id += 1
        return f"{self.sender}-{int(time.time())}-{self.next_id}"[:32]

    # Reading: frames are delimited by BodyLength, so a message is never split or merged.
    def reader(self):
        buf = b""
        while self.open:
            try:
                chunk = self.sock.recv(65536)
            except OSError:
                break
            if not chunk:
                print("\n[connection closed by the venue]")
                self.open = False
                break
            buf += chunk
            while True:
                start = buf.find(b"8=FIX")
                length_at = buf.find(b"\x019=", start)
                if start < 0 or length_at < 0:
                    break
                length_end = buf.find(b"\x01", length_at + 3)
                if length_end < 0:
                    break
                total = length_end + 1 + int(buf[length_at + 3:length_end]) + 7
                if len(buf) < total:
                    break
                frame, buf = buf[start:total].decode(errors="replace"), buf[total:]
                self.received([tuple(f.split("=", 1)) for f in frame.split(SOH) if "=" in f])

    def received(self, fields):
        tags = dict(fields)
        kind = tags.get("35")
        if kind == "1":
            self.send("0", [("112", tags.get("112", ""))])
            return
        if kind == "0":
            return
        if kind == "A":
            self.logged_on.set()
        if kind == "8":
            what = EXEC_TYPES.get(tags.get("150"), tags.get("150"))
            side = "buy" if tags.get("54") == "1" else "sell"
            line = f"{what:9} {tags.get('11', '')}  {side} {tags.get('38', '')} {tags.get('55', '')} @ {tags.get('44', '')}"
            if tags.get("150") == "F":
                role = {"1": "maker", "2": "taker"}.get(tags.get("851"), "")
                line += f"  filled {tags.get('32')} @ {tags.get('31')} ({role})"
            line += f"  leaves {tags.get('151', '')} cum {tags.get('14', '')}  order {tags.get('37', '')}"
            if "58" in tags:
                line += f"  — {tags['58']}"
            print(f"\n<- {line}")
        elif kind == "y":
            print(f"\n<- SecurityList: {tags.get('146', '0')} markets")
            entry = {}
            for k, v in fields:
                if k == "55" and entry:
                    self.print_market(entry)
                    entry = {}
                if k in ("55", "107", "965", "561", "311"):
                    entry[k] = v
            if entry.get("55"):
                self.print_market(entry)
        else:
            shown = {k: v for k, v in fields if k not in ("8", "9", "10", "34", "49", "52", "56")}
            print(f"\n<- {NAMES.get(kind, kind)} {shown}")
        print("> ", end="", flush=True)

    @staticmethod
    def print_market(entry):
        state = "open  " if entry.get("965") == "1" else "closed"
        print(f"   {entry.get('55'):>20}  {state}  lot {entry.get('561', '?'):>3}  {entry.get('107', '')}")

    def heartbeats(self):
        while self.open:
            time.sleep(1)
            if time.monotonic() - self.last_sent >= self.heartbeat:
                try:
                    self.send("0", [])
                except OSError:
                    break

    def order(self, side, args):
        if len(args) < 3:
            return print("usage: buy|sell SYMBOL QTY PRICE [gtc|ioc|fok] [post]")
        symbol, qty, price = args[:3]
        extra = [a.lower() for a in args[3:]]
        clordid = self.clordid()
        fields = [("11", clordid), ("55", symbol), ("54", side), ("60", now()), ("38", qty), ("40", "2"), ("44", price),
                  ("59", next((TIF[a] for a in extra if a in TIF), "1"))]
        if "post" in extra:
            fields.append(("18", "6"))
        self.orders[clordid] = (symbol, side)
        self.send("D", fields)
        print(f"-> NewOrderSingle {clordid}")

    def command(self, line):
        parts = line.split()
        if not parts:
            return True
        name, args = parts[0].lower(), parts[1:]
        if name in ("quit", "exit"):
            self.send("5", [])
            time.sleep(1)
            return False
        if name == "help":
            print(__doc__.split("Commands")[1])
        elif name == "list":
            self.send("x", [("320", self.clordid()), ("559", "4")])
        elif name in ("buy", "sell"):
            self.order("1" if name == "buy" else "2", args)
        elif name in ("cancel", "replace"):
            if not args or args[0] not in self.orders:
                return print(f"no order {args[0] if args else ''} placed in this console") or True
            symbol, side = self.orders[args[0]]
            clordid = self.clordid()
            fields = [("41", args[0]), ("11", clordid), ("55", symbol), ("54", side), ("60", now())]
            if name == "replace":
                if len(args) < 3:
                    return print("usage: replace CLORDID QTY PRICE") or True
                fields += [("38", args[1]), ("40", "2"), ("44", args[2])]
            self.orders[clordid] = (symbol, side)
            self.send("G" if name == "replace" else "F", fields)
            print(f"-> {'OrderCancelReplaceRequest' if name == 'replace' else 'OrderCancelRequest'} {clordid}")
        elif name == "cancelall":
            fields = [("11", self.clordid()), ("530", "1" if args else "7"), ("60", now())]
            if args:
                fields.append(("55", args[0]))
            self.send("q", fields)
        elif name == "raw":
            pairs = [tuple(p.split("=", 1)) for p in " ".join(args).split("|") if "=" in p]
            msg_type = dict(pairs).get("35")
            if not msg_type:
                return print("raw needs 35=") or True
            self.send(msg_type, [p for p in pairs if p[0] != "35"])
        elif name == "sleep":
            time.sleep(float(args[0]) if args else 1)
        else:
            print(f"unknown command {name}; type help")
        return True


def main():
    p = argparse.ArgumentParser(description="A FIX 4.4 console for the venue's FIX adapter.")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=17420)
    p.add_argument("--token", required=True,
                   help="an API key's keyId.secret, or the account's dev token, sent in Password (554)")
    p.add_argument("--sender", default="CONSOLE", help="your SenderCompID")
    p.add_argument("--target", default="VENUE", help="the venue's CompID")
    p.add_argument("--heartbeat", type=int, default=30)
    a = p.parse_args()
    s = Session(a.host, a.port, a.sender, a.target, a.token, a.heartbeat)
    threading.Thread(target=s.reader, daemon=True).start()
    s.send("A", [("98", "0"), ("108", str(a.heartbeat)), ("141", "Y"), ("554", a.token)])
    threading.Thread(target=s.heartbeats, daemon=True).start()
    if not s.logged_on.wait(10):
        sys.exit("no Logon from the venue within 10 s (a refused token is answered with a Logout, printed above)")
    interactive = sys.stdin.isatty()
    while s.open:
        if interactive:
            print("> ", end="", flush=True)
        line = sys.stdin.readline()
        if not line:
            s.command("quit")
            break
        if not interactive and line.strip():
            print(f"> {line.strip()}")
        if not s.command(line.strip()):
            break


if __name__ == "__main__":
    main()
