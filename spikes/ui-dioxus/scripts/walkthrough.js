const { chromium } = require('playwright');
const out = process.argv[2];
const log = (...a) => console.log(...a);
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2 });
  p.on('pageerror', e => log('pageerror:', e.message));
  await p.goto('http://127.0.0.1:18081/');
  await p.waitForSelector('.grid-row', { timeout: 90000 });
  await p.waitForTimeout(300);
  const status = async () => (await p.textContent('.statusbar span')).trim();
  log('start:', await status());
  await p.screenshot({ path: `${out}/1-grid.png` });

  // Keyboard: focus grid, move down 3 and right 1, then back to name.
  await p.click('.grid-row >> nth=0 >> .cell >> nth=0');
  for (const k of ['ArrowDown', 'ArrowDown', 'ArrowDown', 'ArrowRight', 'ArrowLeft']) await p.keyboard.press(k);
  log('active cell after ↓↓↓→←:', await p.textContent('.cell.active'), '| aria-rowindex', await p.getAttribute('.active-row', 'aria-rowindex'));

  // Inline edit by typing, save with Enter.
  const before = await p.textContent('.cell.active');
  await p.keyboard.type('Zeppelin Feinmechanik GmbH');
  await p.screenshot({ path: `${out}/2-editing.png` });
  await p.keyboard.press('Enter');
  await p.waitForSelector('.cell-input', { state: 'detached' });
  log('edited:', JSON.stringify(before), '->', await p.textContent('.grid-row[aria-rowindex="5"] .cell >> nth=0'), '| active now row', await p.getAttribute('.active-row', 'aria-rowindex'));

  // Validation error: empty name stays in edit mode with the API's message.
  await p.keyboard.press('F2');
  await p.fill('.cell-input', '   ');
  await p.keyboard.press('Enter');
  await p.waitForSelector('.cell-input.invalid');
  log('empty name ->', (await p.textContent('.statusbar .error')).trim(), '| input focused:', await p.evaluate(() => document.activeElement?.classList.contains('cell-input')));
  await p.screenshot({ path: `${out}/3-error.png` });
  await p.keyboard.press('Escape');

  // Ctrl+End: jump to the last loaded row, more pages load on the way.
  const t0 = Date.now();
  let rounds = 0;
  while (!(await status()).startsWith('10000 Partner geladen ·') && rounds < 200) {
    await p.focus('.viewport');
    await p.keyboard.press('Control+End');
    await p.waitForTimeout(60);
    rounds++;
  }
  log(`Ctrl+End until all loaded: ${Date.now() - t0} ms, ${rounds} presses ->`, await status());
  await p.keyboard.press('Control+End');
  await p.waitForTimeout(400);
  log('last row (aria-rowindex):', await p.getAttribute('.active-row', 'aria-rowindex'));
  await p.screenshot({ path: `${out}/4-end.png` });

  // Smooth scrolling: wheel through the whole list, measure frame intervals.
  await p.evaluate(() => document.querySelector('.viewport').scrollTop = 0);
  const frames = await p.evaluate(async () => {
    const vp = document.querySelector('.viewport');
    const times = [];
    let last = performance.now();
    return await new Promise(resolve => {
      function step(now) {
        times.push(now - last); last = now;
        vp.scrollTop += 400;  // ~12 rows per frame
        if (vp.scrollTop + vp.clientHeight >= vp.scrollHeight - 1) resolve(times.slice(1));
        else requestAnimationFrame(step);
      }
      requestAnimationFrame(step);
    });
  });
  frames.sort((a, b) => a - b);
  const q = f => frames[Math.floor(frames.length * f)].toFixed(1);
  log(`scroll through 10000 rows: ${frames.length} frames, median ${q(0.5)} ms, p95 ${q(0.95)} ms, max ${frames.at(-1).toFixed(1)} ms`);
  log('rows in DOM:', await p.locator('.grid-row').count());

  // Sort by e-mail descending, filter.
  await p.click('.header-cell:has-text("E-Mail")');
  await p.click('.header-cell:has-text("E-Mail")');
  await p.waitForTimeout(800);
  log('sort -email first row:', await p.textContent('.grid-row[aria-rowindex="2"]'));
  await p.fill('input[type=search]', 'feinkost');
  await p.selectOption('select', 'supplier');
  await p.waitForTimeout(1000);
  log('filter feinkost/supplier:', await status());
  await p.screenshot({ path: `${out}/5-filter.png` });

  // dioxus-tabular tab.
  await p.fill('input[type=search]', '');
  await p.selectOption('select', '');
  await p.click('text=dioxus-tabular');
  await p.waitForFunction(() => /Partner in \d+ ms geladen/.test(document.body.innerText), null, { timeout: 60000 });
  log('tabular:', await p.textContent('.toolbar.slim .hint'));
  let t = Date.now();
  await p.click('.sort-button:has-text("Name")');
  await p.waitForFunction(() => document.querySelector('.sort-button').textContent.includes('▲'));
  log(`tabular sort by name (windowed): ${Date.now() - t} ms`);
  await p.screenshot({ path: `${out}/6-tabular.png` });
  await p.uncheck('.check input');
  t = Date.now();
  await p.waitForFunction(() => document.querySelectorAll('table.tabular tbody tr').length > 9000, null, { timeout: 120000 });
  log(`tabular render all rows: ${Date.now() - t} ms`);
  t = Date.now();
  await p.click('.sort-button:has-text("Name")');
  await p.waitForFunction(() => document.querySelector('.sort-button').textContent.includes('▼'), null, { timeout: 120000 });
  log(`tabular toggle sort with all rows in DOM: ${Date.now() - t} ms`);

  // AG Grid tab.
  await p.click('text=AG Grid (JS)');
  await p.waitForSelector('.ag-row', { timeout: 60000 });
  await p.waitForTimeout(500);
  log('ag grid:', await p.textContent('.statusbar span'));
  await p.click('.ag-row[row-index="0"] [col-id="name"]');
  await p.keyboard.press('ArrowDown');
  await p.keyboard.press('Enter');
  await p.keyboard.press('Control+a');
  await p.keyboard.type('AG Edit Test GmbH');
  await p.keyboard.press('Enter');
  await p.waitForFunction(() => document.body.innerText.includes('gespeichert: AG Edit Test GmbH'), null, { timeout: 10000 });
  log('ag edit:', await p.textContent('.statusbar span'));
  await p.click('.ag-header-cell[col-id="name"]');
  await p.waitForTimeout(1000);
  await p.screenshot({ path: `${out}/7-aggrid.png` });
  const t1 = Date.now();
  await p.evaluate(() => { const v = document.querySelector('.ag-body-viewport'); v.scrollTop = v.scrollHeight; });
  await p.waitForTimeout(3000);
  log(`ag grid after jump to end (${Date.now() - t1} ms):`, await p.textContent('.statusbar span'));
  await b.close();
})().catch(e => { console.error(e); process.exit(1); });
