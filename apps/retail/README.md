# The retail app

One project for three fronts: the web, iOS and Android, from the same screens. Expo (React Native with
`react-native-web`) and Expo Router, which is what makes that one project rather than three.

What is here today is a slice, not a product: the markets, one market with its book and a ticket to trade it, and an
account screen that signs a member in and shows what the venue pushes about them. It exists to prove the shape —
including that a person can sign in (`interfaces/gateway/README.md`, members and sessions) — not to be shipped.

```sh
npm install
npm start                       # then w for the browser, i for a simulator, a for an emulator
npm test                        # the venue client, in Node: no phone, no Metro
npm run typecheck
npm run build:web               # a static site in dist/
```

## What is shared, and what is not

| | |
|---|---|
| `src/venue/` | The venue, in TypeScript and nothing else: the REST client, the private stream, the types and where a session is kept. No React. This is what `npm test` tests, and what a different front end would keep if the screens were thrown away. |
| `src/app/` | The screens, as Expo Router's files: `index` (markets), `market/[id]` (book and ticket), `account` (sign in, balances, resting orders). |
| `src/venue/storage.ts` and `storage.web.ts` | The one real difference between the platforms: a phone keeps the session in the keychain or keystore (`expo-secure-store`), behind the phone's own lock, and a browser keeps it in `localStorage`, because a browser has neither a keychain nor a reader. The bundler picks the file; nothing else in the app knows, and `unlockKind()` answers `"none"` on the web so the screens simply do not offer it. |

Responsiveness is measured, not guessed: the markets list is one column on a phone and as many as fit a wide window,
from `useWindowDimensions`. There is no phone-versus-desktop branch anywhere.

## Signing in

Signing in is a few steps, and which of them a person sees depends on what the venue answers rather than on what the
app guesses (interface 1 README, members and sessions):

- **Signing up** answers only "check your mail", never whether the address is already somebody's, so the app asks for
  the code from that message rather than pretending a session came back.
- **Until the address is proved** a session may look and trade but not move money. The account screen says so and
  takes the code.
- **A second factor** shows up as `SECOND_FACTOR_REQUIRED` on a login whose password was right — the one refusal that
  asks for something different rather than the same thing again — so the app reveals a code field instead of saying
  the password was wrong. A recovery code goes in the same field.
- **A forgotten password** is set again with a code, and doing it signs every session out, this one among them.
- **The sessions a member holds** are listed on the account screen, each with the name the app gave for the device it
  runs on, and any but the current one can be ended from there.

### Unlocking with a fingerprint

On a phone the session is kept behind the device's own lock: `expo-secure-store` with `requireAuthentication`, so
reading it asks for the fingerprint, the face or the passcode. The app does not ask at launch — it draws a screen
offering to unlock, and the member asks for the prompt, because a prompt nobody asked for is the fastest way to teach
people to dismiss one.

**What this is, and what it is not.** It unlocks a credential this device already holds. It is not how the venue
knows who the member is: the venue only ever sees the session credential, exactly as it would if the password had
been typed, and it is not told that a fingerprint was used. Anything the venue itself must be sure of belongs in the
second factor instead.

**It can stop working, by design.** A keystore entry that asks for biometrics is invalidated when the enrolled
fingerprints or faces change, because the phone can no longer say the same person is asking. The read then fails, and
the app forgets the entry and asks for the password — which is the honest answer, not a bug to work around.

**Android asks sooner than iOS.** On Android a locked entry needs authentication for every operation, where iOS asks
only when reading or updating one that already exists. So on Android the prompt appears at the end of *logging in*,
when the session is written down, and not only when a later launch unlocks it. It looks like a bug the first time
and is not one.

**A simulator cannot test any of this.** `expo-secure-store` says so itself: emulators and simulators do not require
biometric authentication when retrieving a secret, so they hand the session back with no prompt and the flow appears
to pass without ever having been exercised. A real device is the only way to see it work.

There is no web equivalent, and the app does not pretend otherwise.

## Pointing it at a venue

`EXPO_PUBLIC_VENUE_URL` says where the venue is; without it the app talks to `http://127.0.0.1:8080`, which is the
laptop demo (`deploy/README.md`, on a laptop).

```sh
EXPO_PUBLIC_VENUE_URL=https://venue.local npm start
```

For a build onto a device, put it in `.env.local` beside `package.json` instead, which the Expo CLI reads and the
repository ignores — an address on somebody's own network is theirs, not the project's:

```sh
echo 'EXPO_PUBLIC_VENUE_URL=http://<the venue's address>:8088' > .env.local
```

A development build is a debug build, and React Native's gradle plugin allows cleartext traffic in debug (and
forbids it in release), so plain HTTP works on Android while testing without any extra configuration. A release
build needs the certificate below.

Two things the venue must be told before a front end of its own can reach it, both in `deploy/README.md` under *from
a phone*: the origin this app is served from (`gateway.allowedOrigins`, or `--allowed-origins`), because a browser
will not otherwise let the app read the answer; and a certificate (`ingress.tls.secretName`), because iOS refuses
cleartext by default and Android has since 9. A simulator reaches the machine at its address on the network; an
Android emulator reaches its host as `10.0.2.2`; neither can use `127.0.0.1`, which is the device itself.

## What it does not do yet

Deposits (the venue's own page does them), parlays, settlement, market search, price history, push notifications, and
anything a store would ask for before it listed a real-money app.

The codes that prove an address and set a password arrive by mail, and a test venue has no mail provider: the
messages sit in the identity service's outbox and the venue's own page lists them (`GET /v4/mail`, on only where the
service was started with `--show-mail`). This app does not read that list — it asks for the code, as it would if a
real message had arrived — so testing a sign-up against a demo venue means reading the code off that page.
