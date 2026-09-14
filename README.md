# Prediction Exchange Blueprint

Architecture spec for the new ProphetX prediction-markets exchange.

- **Published:** https://claude.ai/code/artifact/5e4d2278-46dd-4f2c-a617-da87c3ae0831 (private; every published version is kept there)
- **Source:** `prediction-exchange.html` is the file that page is published from. Edit this, then republish to the same URL.

## Contents

| Path | What it is |
|---|---|
| `prediction-exchange.html` | The blueprint, 23 sections, 18 figures |
| `validate.py` | Structure check: sections, rail, markers, SVG markers, tag balance. Must print PASS |
| `verify_sweep.py` | Consistency sweep: table columns, classes, quotes, dangling section refs, stray characters |
| `geom16.py`, `rename_check.py` | Figure label geometry checks (edit the `(section, figure)` tuple to choose figures) |
| `chrome-widths.js`, `chrome-check.js` | Real-Chromium checks: sideways overflow at 1280/400/360px, screenshots |
| `render-figs.js`, `font-plex-ttf/`, `entities.json` | resvg figure renderer, the IBM Plex Mono fonts and the HTML entity table it uses |
| `prototypes/maskbench/` | Benchmark of bid/ask occupancy masks vs signed quantities |
| `prototypes/l3book/` | L3 order book with a pre-sized slab, differential and capacity tests |
| `history/` | The HTML before each major revision, oldest first |

## Running the checks

```sh
python3 validate.py && python3 verify_sweep.py
(cd pw-tool && npm ci)        # Playwright, for the Chromium checks
(cd render-tool && npm ci)    # resvg, for render-figs.js
node chrome-widths.js
node render-figs.js font-plex-ttf   # figures to PNG; reads entities.json
```

The Chromium scripts expect Playwright's Chromium build 1228 at
`~/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome`.
