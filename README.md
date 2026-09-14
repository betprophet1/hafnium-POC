# Prediction exchange

Architecture, specifications and the core prototype for the new ProphetX prediction-markets exchange.

| Path | What it is | Changed by |
|---|---|---|
| [`blueprint/`](blueprint/) | The architecture blueprint: why the system is shaped the way it is, and the tools that check the document | People |
| [`interfaces/`](interfaces/) | The contracts between components: a catalogue of every interface and the journal schema | People, as reviewed specification changes |
| [`spec/engine/`](spec/engine/) | What one sequencer group's engine does: numbered rules, decisions, hand-computed scenarios, a reference model | People, as reviewed specification changes |
| [`core/`](core/) | The Rust core: engine, single-member node, test harness, generated codec, prototypes, CI gates and the work packages that build it | People and coding agents, against the gates |
| [`deploy/`](deploy/) | The mini-venue Helm chart, the core's image and the one-command laptop cluster | People and coding agents |
| [`AGENTS.md`](AGENTS.md) | How to work here, including what an implementation may never change | — |

The published blueprint is at https://claude.ai/code/artifact/5e4d2278-46dd-4f2c-a617-da87c3ae0831.

## Checking everything

```sh
./check
```

That runs, in order: the blueprint's structure, consistency and figure checks; a gate that no change touches the
engine together with the specification; the interface catalogue and a check that the committed codec matches the
schema; the reference model against every scenario and on random sequences; the Rust formatting, lints and tests;
the Rust engine against the scenarios and the reference model's differential vectors; and a golden replay through
the node.

**Needed:** Python 3.11+, Rust with `rustfmt` and `clippy`. **Optional:** Node, for the Chromium checks
(`cd blueprint/tools && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm ci`); a JDK 17+ and sbe-all 1.40.1, only when the
journal schema changes (see `interfaces/README.md`).
