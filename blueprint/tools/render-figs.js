// Render every <figure> SVG in the page to PNG, in light and dark, with the page's real tokens and font.
const fs = require('fs');
const { Resvg } = require('@resvg/resvg-js');
const path = require('path');
const [,, fontDirArg, only, vbOverride, suffix = ''] = process.argv;
const fontDir = fontDirArg ? path.resolve(fontDirArg) : path.join(__dirname, 'fonts');
process.chdir(__dirname);
fs.mkdirSync('out', { recursive: true });   // only: name filter; vbOverride: 'x y w h' crop
// resvg 2.6 treats orient="auto-start-reverse" as "auto", so a start head would point inward. Emulate the spec:
// drop marker-start and add an invisible reversed copy of the path that carries the same marker as marker-end.
const reverseStart = svg => {
  // For each marker with orient="auto-start-reverse", add a copy rotated 180 degrees about its viewBox centre
  // (reference point reflected, orient="auto") and point every marker-start at the copy. That is exactly the
  // rotation the spec applies at a path start, with no extra geometry for the renderer to optimise away.
  for (const [whole, id, pre, post, inner] of [...svg.matchAll(/<marker id="([^"]+)"([^>]*)orient="auto-start-reverse"([^>]*)>([\s\S]*?)<\/marker>/g)]) {
    const attrs = `${pre} ${post}`;
    const [vx, vy, vw, vh] = (attrs.match(/viewBox="([^"]+)"/) || [, '0 0 8 8'])[1].trim().split(/[\s,]+/).map(Number);
    const cx = vx + vw / 2, cy = vy + vh / 2;
    const refX = Number((attrs.match(/refX="([^"]+)"/) || [, 0])[1]), refY = Number((attrs.match(/refY="([^"]+)"/) || [, 0])[1]);
    const revAttrs = attrs.replace(/refX="[^"]+"/, `refX="${2 * cx - refX}"`).replace(/refY="[^"]+"/, `refY="${2 * cy - refY}"`);
    const rev = `<marker id="${id}__rev"${revAttrs} orient="auto"><g transform="rotate(180 ${cx} ${cy})">${inner}</g></marker>`;
    svg = svg.replace(whole, whole.replace('orient="auto-start-reverse"', 'orient="auto"') + rev)
             .split(`marker-start="url(#${id})"`).join(`marker-start="url(#${id}__rev)"`);
  }
  return svg;
};
const html = fs.readFileSync('../prediction-exchange.html', 'utf8');
const css = html.slice(0, html.indexOf('</style>'));
const tokens = block => Object.fromEntries([...block.matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)].map(m => [m[1], m[2].trim()]));
const light = tokens(css.match(/(?<![\w\]\)]):root\s*\{([^}]*)\}/)[1]);
const dark  = { ...light, ...tokens(css.match(/:root\[data-theme="dark"\]\s*\{([^}]*)\}/)[1]) };
const ENT = JSON.parse(fs.readFileSync('entities.json', 'utf8'));   // full HTML5 named-entity table
const fonts = fs.readdirSync(fontDir).filter(f => /\.ttf$/i.test(f)).map(f => `${fontDir}/${f}`);
const family = fonts.some(f => /Plex/i.test(f)) ? 'IBM Plex Mono' : 'DejaVu Sans Mono';
const secAt = i => { let n = 0; for (const m of html.slice(0, i).matchAll(/<section id="s(\d+)">/g)) n = m[1]; return String(n).padStart(2, '0'); };
const perSec = {};
for (const fm of html.matchAll(/<figure>[\s\S]*?<\/figure>/g)) {
  const sec = secAt(fm.index); perSec[sec] = (perSec[sec] || 0) + 1;
  const svg = fm[0].match(/<svg[\s\S]*?<\/svg>/)[0];
  for (const [theme, T] of [['light', light], ['dark', dark]]) {
    const name = `fig-s${sec}-${perSec[sec]}-${theme}${suffix}`;
    if (only && !name.includes(only)) continue;
    const v = s => s.replace(/var\((--[\w-]+)\)/g, (_, k) => { if (!(k in T)) throw new Error(`${name}: unknown token ${k}`); return T[k]; });
    const style = v(`svg { color: var(--ink-2); }
      text { font-family: '${family}'; fill: currentColor; }
      .box { fill: none; stroke: currentColor; stroke-width: 1; opacity: 0.55; }
      .box-fill { fill: var(--surface-2); stroke: var(--rule-2); stroke-width: 1; }
      .lbl-hd { font-size: 12px; font-weight: 500; fill: var(--ink); }
      .lbl-sub { font-size: 10.5px; fill: var(--ink-3); }
      .lbl-edge { font-size: 10px; fill: var(--ink-3); }
      .wire { fill: none; stroke: currentColor; stroke-width: 1.2; opacity: 0.65; }
      .wire-yes { fill: none; stroke: var(--yes); stroke-width: 1.4; }
      .wire-no { fill: none; stroke: var(--no); stroke-width: 1.4; }
      .dash { stroke-dasharray: 4 3; }`);
    let body = (process.env.NO_REVERSE ? svg : reverseStart(svg)).replace(/&([A-Za-z][A-Za-z0-9]*);/g, (m, e) => { if (!(e in ENT)) throw new Error(`${name}: unknown entity ${m}`); return [...ENT[e]].map(c => `&#${c.codePointAt(0)};`).join(''); });
    body = v(body).replace(/<svg\b([^>]*)>/, (m, a) => `<svg${/xmlns=/.test(a) ? '' : ' xmlns="http://www.w3.org/2000/svg"'}${a} color="${T['--ink-2']}"><style>${style}</style>`);
    if (vbOverride) body = body.replace(/viewBox="[^"]*"/, `viewBox="${vbOverride}"`);
    const png = new Resvg(body, { fitTo: { mode: 'width', value: vbOverride ? 1300 : 1800 }, background: T['--surface'],
      font: { fontFiles: fonts, loadSystemFonts: false, defaultFontFamily: family, monospaceFamily: family } }).render().asPng();
    fs.writeFileSync(`out/${name}.png`, png);
    console.log(`${name}.png  ${(png.length / 1024).toFixed(0)} KB`);
  }
}
console.log('font family used:', family, '| files:', fonts.length);
