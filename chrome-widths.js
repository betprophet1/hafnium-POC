// Width pass in real Chromium: sideways overflow, outermost offenders, gutters, and the §10 lifecycle chain.
const { chromium } = require('./pw-tool/node_modules/playwright');
const path = require('path');
const exe = `${process.env.HOME}/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome`;
const url = 'file://' + path.resolve('prediction-exchange.html');
(async () => {
  const browser = await chromium.launch({ executablePath: exe, args: ['--no-sandbox'] });
  for (const width of [1280, 400, 360]) {
    const ctx = await browser.newContext({ colorScheme: 'light', viewport: { width, height: 860 }, deviceScaleFactor: 2 });
    const page = await ctx.newPage();
    await page.goto(url, { waitUntil: 'networkidle', timeout: 60000 });
    await page.evaluate(() => document.fonts.ready);
    const r = await page.evaluate(() => {
      const vw = window.innerWidth;
      const clipped = el => { for (let a = el.parentElement; a && a !== document.body; a = a.parentElement) {
        const o = getComputedStyle(a).overflowX; if (o === 'auto' || o === 'scroll' || o === 'hidden') return true; } return false; };
      const off = [];
      for (const el of document.querySelectorAll('body *')) {
        const b = el.getBoundingClientRect(); if (!b.width) continue;
        if ((b.right > vw + 1 || b.left < -1) && !clipped(el)) {
          const pb = el.parentElement.getBoundingClientRect(); if (pb.right > vw + 1 || pb.left < -1) continue;
          off.push(`${el.tagName.toLowerCase()} in ${(el.closest('section') || {}).id || '-'}: ${Math.round(b.left)}..${Math.round(b.right)} "${(el.textContent || '').trim().slice(0, 40)}"`);
        }
      }
      const code = [...document.querySelectorAll('code')].find(c => c.textContent.startsWith('draft'));
      const cb = code.getBoundingClientRect(), lh = parseFloat(getComputedStyle(code).lineHeight) || 20;
      const ps = [...document.querySelectorAll('section p')].map(p => p.getBoundingClientRect()).filter(b => b.width);
      return { vw, overflowPx: document.documentElement.scrollWidth - vw, offenders: off,
        gutterL: Math.round(Math.min(...ps.map(b => b.left))), gutterR: Math.round(vw - Math.max(...ps.map(b => b.right))),
        lifecycle: { whiteSpace: getComputedStyle(code).whiteSpace, lines: code.getClientRects().length, right: Math.round(cb.right) } };
    });
    console.log(`width ${width}: sideways overflow ${r.overflowPx}px | gutters ${r.gutterL}/${r.gutterR}px | offenders ${r.offenders.length}` +
      ` | lifecycle code white-space=${r.lifecycle.whiteSpace}, line boxes=${r.lifecycle.lines}, right edge=${r.lifecycle.right}`);
    r.offenders.slice(0, 5).forEach(o => console.log('   - ' + o));
    if (width !== 1280) {
      const para = await page.evaluateHandle(() => [...document.querySelectorAll('code')].find(c => c.textContent.startsWith('draft')).closest('p'));
      await para.asElement().scrollIntoViewIfNeeded();
      await para.asElement().screenshot({ path: `chrome-out/phone-${width}-s10-lifecycle.png` });
    }
    await ctx.close();
  }
  await browser.close();
})().catch(e => { console.error('FAILED:', e.message.split('\n').slice(0, 5).join('\n')); process.exit(1); });
