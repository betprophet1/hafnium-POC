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
| `sweep-epoll.txt`, `sweep-books.txt` | Retail population sweeps from a separate client machine: the first with every customer on all eight books, the second with the corrected model (retail on two weighted books, eight API clients on all of them). |
| `sweep-2host.txt` | The same before the epoll client, kept because its `client_load` column is what showed the generator, not the gateway, was the limit. |
| `lockdelta-16384.txt`, `lockdelta-16384-after.txt`, `lockdelta-books.txt` | Lock cost as before/after deltas over one 60s run: conflate hold, fan-out, read wait and feed-apply. In order: before the fan-out fix, after it, and after the subscription model was corrected too. |
| `conflate-profile.txt` | `perf` on the public `gw-conflate` thread at 16,384 subscribers: the profile that found `eventfd_write` at 51.7% and cleared the btree lookup of suspicion. |
| `lockdelta-wake.txt` | Lock cost after the wake coalescing: fan-out 10.7ms, conflation 9.2/s. |
| `retail.txt` | The first retail sweep, with the generator and the gateway sharing one box. Client-bound above ~1,024; kept only because the corrections reference it. |

## The scripts

`real.sh` / `allreal.sh` produce the representative runs; `prof.sh` the profiles; `launch2.sh` /
`provision2.sh` / `server-up.sh` / `client-sweep.sh` the two-host runs, and `lockdelta.sh` the lock deltas. `real_agg.py`,
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
- **In `sweep-epoll.txt` and everything before it, every retail customer subscribes to all eight books**, which
  is an API client's shape rather than an app user's, so their fan-out cost is overstated by roughly the number
  of books. `sweep-books.txt` and `lockdelta-books.txt` use the corrected model: retail on two weighted books,
  `--watchers` on all of them.
- **`retail.txt` and `sweep-2host.txt` are client-bound above about a thousand customers**, which the
  `client_load` column shows. Only the runs made with the epoll pool (`21b9beb` onward) measure the gateway.
