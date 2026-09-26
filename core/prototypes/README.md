# Prototypes

Experiments that back claims in the blueprint. They build as part of the workspace so they do not rot, but they
are not held to the engine's gates (no `no_std`, formatting and lints not enforced) and nothing may depend on them.

| Crate | What it shows | Run |
|---|---|---|
| `l3book` | The §6 layout: order slab with free list, id map with backward-shift deletion, per-tick intrusive FIFOs and occupancy masks, checked against an ordered-map model; `l3cap` adds the full-slab behaviour | `cargo run --release -p proto-l3book --bin l3` |
| `feedapply` | What the gateway's feed path pays to read a result through `serde_json::Value` instead of the typed SBE decoder it is built on, for ADR-0002 | `cargo run --release -p proto-feedapply` |
| `maskbench` | Occupancy bitmaps against scanning the level array, at 1¢ and 0.1¢ ticks | `cargo run --release -p proto-maskbench` |

Work package WP-2 starts from `l3book`: learn from it, do not import it.
