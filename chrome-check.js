// Render the real page in Chromium: every figure in light and dark, fonts actually loaded, and a 400px phone pass.
const { chromium } = require('./pw-tool/node_modules/playwright');
const fs = require('fs'), path = require('path');
const exe = process.env.CHROME || `${process.env.HOME}/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome`;
const url = 'file://' + path.resolve('prediction-exchange.html');
(async () => {
  const browser = await chromium.launch({ executablePath: exe, args: ['--no-sandbox'] });
  console.log('launched Chromium', browser.version(), 'from', exe);
  const report = { figures: {} };
  for (const scheme of ['light', 'dark']) {
    const ctx = await browser.newContext({ colorScheme: scheme, viewport: { width: 1280, height: 900 }, deviceScaleFactor: 2 });
    const page = await ctx.newPage();
    await page.goto(url, { waitUntil: 'networkidle', timeout: 60000 });
    await page.evaluate(() => document.fonts.ready);
    if (scheme === 'light') report.fonts = await page.evaluate(() => ({
      plexMono: document.fonts.check('12px "IBM Plex Mono"'), plexSans: document.fonts.check('12px "IBM Plex Sans"'),
      newsreader: document.fonts.check('16px "Newsreader"'),
      loadedFaces: [...new Set([...document.fonts].filter(f => f.status === 'loaded').map(f => `${f.family} ${f.weight}`))] }));
    const figs = await page.$$('figure');
    const seen = {};
    for (const fig of figs) {
      const sec = await fig.evaluate(e => (e.closest('section') || {}).id || 's0');
      const n = (seen[sec] = (seen[sec] || 0) + 1);
      const name = `fig-${sec.replace(/^s(\d)$/, 's0$1')}-${n}-${scheme}`;
      await fig.scrollIntoViewIfNeeded();
      await fig.screenshot({ path: `chrome-out/${name}.png` });
      report.figures[name] = true;
    }
    await ctx.close();
  }
  // phone width
  const ctx = await browser.newContext({ colorScheme: 'light', viewport: { width: 400, height: 860 }, deviceScaleFactor: 2 });
  const page = await ctx.newPage();
  await page.goto(url, { waitUntil: 'networkidle', timeout: 60000 });
  await page.evaluate(() => document.fonts.ready);
  report.phone = await page.evaluate(() => {
    const vw = window.innerWidth;
    const scrollsX = el => { for (let a = el.parentElement; a && a !== document.body && a !== document.documentElement; a = a.parentElement) {
      const o = getComputedStyle(a).overflowX; if (o === 'auto' || o === 'scroll' || o === 'hidden') return true; } return false; };
    const offenders = [];
    for (const el of document.querySelectorAll('body *')) {
      const r = el.getBoundingClientRect();
      if (!r.width || getComputedStyle(el).display === 'none') continue;
      if ((r.right > vw + 1 || r.left < -1) && !scrollsX(el)) {
        const pr = el.parentElement.getBoundingClientRect();
        if (pr.right > vw + 1 || pr.left < -1) continue;            // report the outermost offender only
        offenders.push({ tag: el.tagName.toLowerCase(), cls: el.className && el.className.baseVal === undefined ? el.className : (el.getAttribute('class') || ''),
          section: (el.closest('section') || {}).id || '-', left: Math.round(r.left), right: Math.round(r.right), text: (el.textContent || '').trim().slice(0, 60) });
      }
    }
    const paras = [...document.querySelectorAll('section p')].map(p => p.getBoundingClientRect()).filter(r => r.width);
    const minLeft = Math.min(...paras.map(r => r.left)), maxRight = Math.max(...paras.map(r => r.right));
    return { viewport: vw, docScrollWidth: document.documentElement.scrollWidth, horizontalOverflowPx: document.documentElement.scrollWidth - vw,
      paragraphGutterLeft: Math.round(minLeft), paragraphGutterRight: Math.round(vw - maxRight), offenders: offenders.slice(0, 25), offenderCount: offenders.length };
  });
  await page.screenshot({ path: 'chrome-out/phone-top.png' });
  const map = (await page.$$('section#s18 figure'))[0];
  if (map) { await map.scrollIntoViewIfNeeded(); await page.screenshot({ path: 'chrome-out/phone-s18-map.png' }); }
  await ctx.close(); await browser.close();
  report.figureShots = Object.keys(report.figures).length; delete report.figures;
  console.log(JSON.stringify(report, null, 2));
})().catch(e => { console.error('FAILED:', e.message.split('\n').slice(0, 6).join('\n')); process.exit(1); });
