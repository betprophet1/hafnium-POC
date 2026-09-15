# Interfaces

One specification per interface between components of the venue, and one catalogue of all of them.

- `catalogue.toml` lists every interface: its number, what it carries, the guarantees it makes, its
  format, its status and its specification file. The two interface tables in the blueprint's §18 are
  generated from it, and `tools/catalogue.py check` fails if the blueprint and the catalogue disagree.
- Interfaces 1–19 are the numbered arrows on §18's map. 20 onwards sit behind boxes the map already draws.
- Interfaces owned by providers, the data vendor, Nasdaq and the CFTC are listed as outside and are not
  specified here.

## Formats

| What | Format | Location |
|---|---|---|
| Journal inputs, engine results, binary messages | SBE 2.0 XML | `sbe/` |
| REST and WebSocket | OpenAPI 3.1, AsyncAPI 3 | `openapi/`, `asyncapi/` (not started) |
| Payment and oracle contracts | Rust traits plus conformance suites | `contracts/` (not started) |
| Stores read by more than one component | ClickHouse DDL, Valkey key layout | `stores/` (not started) |

A store owned by a single service is not an interface. A table or key that two components depend on is.

## Order of work

1. Shared types: identifiers, ticks, quantities, cents, sequence numbers, timestamps. Draft v0 in `sbe/venue-journal.xml`.
2. Journal schema: client messages, sequencer events, service commands and engine results. Draft v0 in `sbe/venue-journal.xml`.
3. Reader contract: exactly-once application and watermarks, for the ledger, ClickHouse, Valkey and surveillance readers.
4. Service commands and the outbox relay.
5. Contracts in the venue's own terms: payment, oracle, dummy oracle control, resolution console, session and auth.
6. Native REST and WebSocket API, including minimum-sequence reads and the environment manifest. Draft v0 of the
   gateway (interface 1) in `gateway/`.
7. Retention exports, signed manifests and the hold register.

Market-maker wire formats (interfaces 2, 3, 4, 5 and the feed side of 8) are drafted but not frozen until
firms have reviewed them.

## Rules for the journal schema

- **Record header.** Every journal input starts with `RecordHeader`. A gateway or service sends it zeroed and
  the sequencer stamps it in place, so ingress bytes and journal bytes are one message.
- **Results name their input.** Every result starts with `ResultHeader`: the input's sequence number and the
  result's position among that input's results. A reader's watermark is an input sequence number.
- **Result log.** The journal records what the engine was told. The journal and replay nodes run the engine
  over the committed journal and archive its results beside it. Replaying a journal regenerates its result
  log exactly.
- **No floats, no personal data.** Prices are tick ordinals for YES, quantities are whole contracts, money is
  integer cents. Account identifiers are pseudonymous.
- **Additive only.** Within a schema version, new fields are appended with `sinceVersion`; nothing is
  removed, retyped or repurposed. Engine behaviour changes through `ActivateProtocol`, never through the
  schema version.
- **Not yet in the schema.** RFQ requests, quotes and acceptances; the snapshot fence and role-change
  messages on interface 10; order-level (L3) market data.

## Generated code

- **Rust:** `interfaces/tools/sbe.sh` regenerates `core/journal`, the codec crate the engine and harness use. It is
  committed, so building the core needs no Java. `core/journal/SCHEMA.sha256` records the schema it was generated
  from, and `./check` fails if the schema has changed since.
- **Java:** generated into `interfaces/generated/java` for the future Aeron clustered service; not committed.
- **JSON codec for the harness:** `interfaces/tools/gen_rust_json_codec.py` writes `core/harness/src/codec.rs`.
- **Round-trip tests** of the generated codec live in `core/harness/tests/journal_roundtrip.rs`.

## Checks

```sh
./check                                                                  # every gate in the repository
python3 interfaces/tools/catalogue.py check                              # catalogue against blueprint and schema
JAVA=/path/to/java SBE_JAR=/path/to/sbe-all-1.40.1.jar interfaces/tools/sbe.sh   # after changing the schema
```

`sbe.sh` needs a JDK 17 or later and `uk.co.real-logic:sbe-all:1.40.1` from Maven Central
(`https://repo1.maven.org/maven2/uk/co/real-logic/sbe-all/1.40.1/sbe-all-1.40.1.jar`, check its `.sha1`).
