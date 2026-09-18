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
| `src/venue/storage.ts` and `storage.web.ts` | The one real difference between the platforms: a phone keeps the session in the keychain or keystore (`expo-secure-store`), a browser in `localStorage`, because a browser has no keychain. The bundler picks the file; nothing else in the app knows. |

Responsiveness is measured, not guessed: the markets list is one column on a phone and as many as fit a wide window,
from `useWindowDimensions`. There is no phone-versus-desktop branch anywhere.

## Pointing it at a venue

`EXPO_PUBLIC_VENUE_URL` says where the venue is; without it the app talks to `http://127.0.0.1:8080`, which is the
laptop demo (`deploy/README.md`, on a laptop).

```sh
EXPO_PUBLIC_VENUE_URL=https://venue.local npm start
```

Two things the venue must be told before a front end of its own can reach it, both in `deploy/README.md` under *from
a phone*: the origin this app is served from (`gateway.allowedOrigins`, or `--allowed-origins`), because a browser
will not otherwise let the app read the answer; and a certificate (`ingress.tls.secretName`), because iOS refuses
cleartext by default and Android has since 9. A simulator reaches the machine at its address on the network; an
Android emulator reaches its host as `10.0.2.2`; neither can use `127.0.0.1`, which is the device itself.

## What it does not do yet

Deposits (the venue's own page does them), parlays, settlement, market search, price history, push notifications, and
anything a store would ask for before it listed a real-money app.
