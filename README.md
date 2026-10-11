# Prediction exchange

Architecture, specifications and the core prototype for the new ProphetX prediction-markets exchange.

| Path | What it is | Changed by |
|---|---|---|
| [`blueprint/`](blueprint/) | The architecture blueprint: why the system is shaped the way it is, and the tools that check the document | People |
| [`interfaces/`](interfaces/) | The contracts between components: a catalogue of every interface and the journal schema | People, as reviewed specification changes |
| [`spec/engine/`](spec/engine/) | What one sequencer group's engine does: numbered rules, decisions, hand-computed scenarios, a reference model | People, as reviewed specification changes |
| [`core/`](core/) | The Rust core: engine, single-member node, test harness, generated codec, prototypes, CI gates and the work packages that build it | People and coding agents, against the gates |
| [`adapters/`](adapters/) | Protocol adapters outside the Rust core: the FIX 4.4 adapter on Artio, in Java | People and coding agents, against the gates |
| [`deploy/`](deploy/) | The mini-venue Helm chart, the core's image and the one-command laptop cluster | People and coding agents |
| [`AGENTS.md`](AGENTS.md) | How to work here, including what an implementation may never change | — |
| [`docs/legacy-comparison.md`](docs/legacy-comparison.md) | How this architecture compares to ProphetX's current production venue: what it fixes, what production still does that this doesn't yet | People |

## Try it

You need no cloud account to run the venue. Pick the first that fits:

1. **On a laptop, no account.** Docker, [k3d](https://k3d.io), Helm and kubectl, then `deploy/laptop.sh up` and
   `deploy/laptop.sh web`, and open http://127.0.0.1:8080. It runs the same chart as everything else, with synthetic
   makers and takers and a dummy oracle, so there are markets moving as soon as it is up (`deploy/README.md`).
2. **On your own AWS account.** `deploy/aws/` makes one small box, DNS and certificates under a domain you own. It
   names no one's account: you give it your own profile, domain and GitHub owner (`deploy/aws/README.md`).
3. **Read it first.** The blueprint says why, `interfaces/` and `spec/engine/` say what, `STATE.md` says where
   state lives, and `core/WORK_PACKAGES.md` says how the core is built and in what order.

The published blueprint is at https://claude.ai/code/artifact/5e4d2278-46dd-4f2c-a617-da87c3ae0831.

`STATE.md` maps where every kind of state lives, what a restart does with it, how it is archived, and how a test
environment is founded on a chosen snapshot rather than on genesis.

## Checking everything

```sh
./check
```

That runs, in order: the blueprint's structure, consistency and figure checks; a gate that no change touches the
engine together with the specification; a check that the tree names no particular person's owner, registry or domain;
a throwaway PostgreSQL in a container for the database tests, when Docker is
usable; the interface catalogue and a check that the committed codec matches the
schema; the reference model against every scenario and on random sequences; the Rust formatting, lints and tests;
the Rust engine against the scenarios and the reference model's differential vectors; a golden replay through
the node; and, when its toolchain is there, the FIX adapter against a local venue.

**Needed:** Python 3.11+, Rust with `rustfmt` and `clippy`, and OpenSSL's headers (`libssl-dev`), which the services'
PostgreSQL client builds against. **Optional:** Docker, for the database tests (`core/store/README.md`; they say they
were skipped without it, or `VENUE_TEST_DATABASE_URL` names a server of your own); Node, for the Chromium checks
(`cd blueprint/tools && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm ci`); a JDK 17+ and sbe-all 1.40.1, only when the
journal schema changes (see `interfaces/README.md`); the FIX adapter's toolchain, fetched into the repository by
`adapters/fix/bootstrap.sh`, for its integration test (see `adapters/fix/README.md`).
