# Measurements behind ADR-0002

Raw output, kept because ADR-0002 states figures that were wrong three times and the corrections are only
checkable against what was actually run. Everything here is from a throwaway c6i.8xlarge spot instance
(32 vCPU) in eu-central-1, gateway with 8 pinned reactor threads, one account per connection.

## The runs

| files | what |
|---|---|
| `real-{prestage,fix}-{1,2,3}.txt` | **The representative load, and the numbers to quote.** 60-second runs, 8 books with exponential popularity, ~10 orders per trade. Arms interleaved; only the gateway binary differs (`ea4724c` vs `74dbc71`), with HEAD's `venue-sim` and `venue-node` driving both. |
| `rep-{prestage,oldhead,fix}-{1,2,3}.txt` | 10-second runs on the **old** 1:1 single-book load, three arms interleaved. This is the round that appeared to show a 25% regression. |
| `rep-{prestage,lazy,typed}-{4,5,6}.txt` | The bisect over that same load: `ea4724c` → `0bf4a7b` (lazy `Frame`) → `f982b3a` (typed `View`). Attributed the apparent loss to `0bf4a7b`. |
| `prof-{prestage,lazy}.txt` | Whole-process `perf` profiles under the old load: per-thread CPU, self time by symbol, by object, by thread, plus the 70-second load result and `feed_apply`. |
| `results-{prestage,head}.txt` | Full connection ladders (4→256) for trading, amend-only and watchers, on the old load. 8-second runs. |

## The scripts

`real.sh` / `allreal.sh` produce the representative runs; `prof.sh` the profiles. `real_agg.py`,
`aggregate.py`, `aggregate3.py` and `bisect_agg.py` aggregate the corresponding files — run them from this
directory.

## Reading these

- **Anything labelled `rep-*`, `results-*` or `prof-*` is on the unrepresentative load**: every taker order
  crossed, one price level, one book. Kept because the corrections reference them, not because the numbers
  describe the venue.
- **`fills/s` in `results-*.txt` is about 2.4× too high.** The counter took the length of `result.executions`,
  which includes `accepted`, `cancelled` and `amended`. Fixed in `2adfa71`; the `real-*` files are correct.
- **8-second runs overstate throughput by roughly a third** against the same load run for 60 seconds.
- The `1:N` in a `real-*` line is the *achieved* order-to-trade ratio, not what was asked for. Eight
  connections over eight books give 1:20, because the exponentially unpopular books get takers and no maker.
