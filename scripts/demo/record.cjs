// Drives the real UI through the demo storyboard with Playwright.
//
//   DEMO_URL        base URL of a `swe serve` started in demo mode
//   DEMO_OUT        directory for the output
//   DEMO_MODE       `video` (default) records a .webm, `shots` takes 2x screenshots
//
// Use record.sh, which prepares the workspace and the server first.

const { chromium } = require('playwright');

const url = process.env.DEMO_URL || 'http://127.0.0.1:4599/';
const out = process.env.DEMO_OUT || '.';
const shots = process.env.DEMO_MODE === 'shots';
const WIDTH = 1440;
const HEIGHT = 900;

// Playwright videos have no mouse pointer; draw one so clicks can be followed.
const CURSOR = `
(() => {
  const cursor = document.createElement('div');
  cursor.style.cssText = [
    'position:fixed', 'left:-40px', 'top:-40px', 'z-index:2147483647', 'width:22px', 'height:22px',
    'pointer-events:none', 'transition:transform .09s ease-out', 'transform-origin:4px 3px',
    'background:no-repeat url("data:image/svg+xml,' + encodeURIComponent(
      '<svg xmlns="http://www.w3.org/2000/svg" width="22" height="22" viewBox="0 0 22 22">' +
      '<path d="M4 3l13 6.5-5.4 1.7L9.6 17z" fill="#fff" stroke="#0b0d10" stroke-width="1.4" stroke-linejoin="round"/></svg>') + '")',
  ].join(';');
  const attach = () => document.body && document.body.appendChild(cursor);
  if (document.body) attach(); else document.addEventListener('DOMContentLoaded', attach);
  document.addEventListener('mousemove', (e) => { cursor.style.left = e.clientX - 4 + 'px'; cursor.style.top = e.clientY - 3 + 'px'; }, true);
  document.addEventListener('mousedown', () => { cursor.style.transform = 'scale(.78)'; }, true);
  document.addEventListener('mouseup', () => { cursor.style.transform = ''; }, true);
})();`;

(async () => {
  const browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: { width: WIDTH, height: HEIGHT },
    colorScheme: 'dark',
    deviceScaleFactor: shots ? 2 : 1,
    ...(shots ? {} : { recordVideo: { dir: out, size: { width: WIDTH, height: HEIGHT } } }),
  });
  if (!shots) await context.addInitScript(CURSOR);
  const page = await context.newPage();
  const problems = [];
  page.on('pageerror', (error) => problems.push(error.message));
  page.on('console', (message) => message.type() === 'error' && problems.push(message.text()));

  // In screenshot mode nothing needs to be watchable, so skip the pauses.
  const pause = (ms) => page.waitForTimeout(shots ? Math.min(ms, 250) : ms);
  const mark = async (name) => {
    if (shots) await page.screenshot({ path: `${out}/${name}.png` });
  };
  let mouse = { x: WIDTH / 2, y: HEIGHT / 2 };
  const point = async (locator) => {
    const box = await locator.boundingBox();
    if (!box) throw new Error('element is not visible');
    const target = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
    const distance = Math.hypot(target.x - mouse.x, target.y - mouse.y);
    await page.mouse.move(target.x, target.y, { steps: shots ? 1 : Math.max(8, Math.round(distance / 22)) });
    mouse = target;
  };
  const click = async (locator) => {
    await locator.waitFor({ state: 'visible', timeout: 30000 });
    await point(locator);
    await pause(220);
    await locator.click();
    await pause(280);
  };
  const type = async (locator, text, delay = 26) => {
    await click(locator);
    await page.keyboard.type(text, { delay: shots ? 0 : delay });
    await pause(350);
  };
  const turnsFinished = (count) =>
    page.waitForFunction((n) => document.querySelectorAll('.turn-end').length >= n, count, { timeout: 120000 });

  await page.goto(url);
  await page.waitForSelector('.sidebar');
  await pause(1300);
  await mark('home');

  // 1. One agent, supervised: it plans, edits, and asks before running commands.
  await click(page.getByRole('button', { name: 'New session' }).first());
  await page.waitForSelector('.dialog');
  await pause(500);
  await click(page.locator('.dialog .chip').first());
  await page.waitForSelector('.picker-item');
  await pause(500);
  await click(page.locator('.picker-item').first());
  await click(page.getByRole('radio', { name: 'Auto-edit', exact: true }));
  await type(page.locator('.dialog textarea'), 'Add a delete method to the note store and test it.');
  await mark('new-session');
  await click(page.getByRole('button', { name: 'Start' }));
  await page.waitForSelector('.session-header');
  await page.locator('.tabs .badge').first().waitFor({ timeout: 30000 });
  await click(page.getByRole('tab', { name: /Plan/ }));
  await page.waitForSelector('.tool-approval', { timeout: 30000 });
  await pause(1100);
  await mark('approval');
  await click(page.getByRole('button', { name: /Always allow/ }));
  await turnsFinished(1);
  await pause(700);
  await click(page.getByRole('tab', { name: /Changes/ }));
  await pause(2200);
  await mark('changes');

  // 2. A follow-up in the same conversation.
  await type(page.getByLabel('Message'), 'Commit it.', 45);
  await page.keyboard.press('Enter');
  await page.waitForSelector('.tool-approval', { timeout: 30000 });
  await pause(800);
  await click(page.getByRole('button', { name: 'Allow', exact: true }));
  await turnsFinished(2);
  await pause(1400);

  // 3. An orchestrator that splits the work across parallel sub-agents.
  await click(page.getByRole('button', { name: 'New session' }).first());
  await page.waitForSelector('.dialog');
  await pause(400);
  await click(page.getByRole('radio', { name: 'Orchestrator', exact: true }));
  await click(page.getByRole('radio', { name: 'Full auto', exact: true }));
  await type(page.locator('.dialog textarea'), 'Add tags and search to the notes store. Split the work across agents.');
  await click(page.getByRole('button', { name: 'Start' }));
  await page.waitForFunction(() => document.querySelector('.session-title')?.textContent?.startsWith('Add tags'), null, {
    timeout: 30000,
  });
  await click(page.getByRole('tab', { name: /Agents/ }));
  // Both workers are running and have started using tools.
  await page.waitForFunction(() => document.querySelectorAll('.timeline.nested .timeline.nested, .timeline.nested .tool').length >= 4, null, {
    timeout: 60000,
  });
  await pause(900);
  await mark('parallel-agents');

  // 4. Every agent on one board, while they keep working.
  await click(page.locator('a.nav-link', { hasText: 'All agents' }));
  await page.waitForSelector('.board .card');
  await pause(1900);
  await mark('board');
  await click(page.locator('.board .card').first());
  await page.waitForSelector('.session-header');
  await turnsFinished(1);
  await pause(900);
  await mark('orchestrated');
  await click(page.getByRole('tab', { name: /Changes/ }));
  await pause(2400);
  await mark('orchestrated-changes');
  await click(page.getByRole('tab', { name: /Plan/ }));
  await pause(1800);
  await click(page.getByRole('tab', { name: /Agents/ }));
  await pause(2200);

  if (shots) {
    await click(page.getByRole('button', { name: 'Settings' }));
    await page.waitForSelector('.dialog');
    await pause(500);
    await mark('settings');
  }

  await context.close();
  if (!shots) {
    const video = await page.video().path();
    console.log(`video: ${video}`);
  }
  await browser.close();
  if (problems.length) {
    console.error('browser errors:', problems);
    process.exit(1);
  }
})().catch((error) => {
  console.error('recording failed:', error.message);
  process.exit(1);
});
