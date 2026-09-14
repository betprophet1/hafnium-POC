# Blueprint

The architecture blueprint for the exchange, and the tools that keep it correct.

- **Published:** https://claude.ai/code/artifact/5e4d2278-46dd-4f2c-a617-da87c3ae0831 (private; every published
  version is kept there).
- **Source:** `prediction-exchange.html`. Edit it here, then republish to the same URL.

| Path | What it is |
|---|---|
| `prediction-exchange.html` | The blueprint |
| `history/` | The document before each major revision, from before it lived in git, oldest first |
| `tools/validate.py` | Structure: section ids, rail, comment markers, SVG markers, CSS tokens, tag balance |
| `tools/verify_sweep.py` | Consistency: table columns, CSS classes, quotation marks, dangling section references, stray characters |
| `tools/figure_geometry.py` | Every figure: labels inside the viewBox, not crossing lines, not overlapping |
| `tools/chrome-widths.js` | Real Chromium: sideways overflow at 1280, 400 and 360 px |
| `tools/chrome-check.js` | Real Chromium: every figure in light and dark, fonts loaded, a phone pass |
| `tools/render-figs.js` | Renders every figure to PNG with resvg, using `tools/fonts/` and `tools/entities.json` |

The §18 interface tables are generated from `interfaces/catalogue.toml`; change the catalogue and run
`python3 interfaces/tools/catalogue.py check` rather than editing those tables by hand.

## Checks

```sh
python3 blueprint/tools/validate.py
python3 blueprint/tools/verify_sweep.py
python3 blueprint/tools/figure_geometry.py
(cd blueprint/tools && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm ci)   # once, for the Chromium and render tools
node blueprint/tools/chrome-widths.js
```

`./check` at the repository root runs all of these. Tool output (screenshots, renders) goes to `tools/out/`.
