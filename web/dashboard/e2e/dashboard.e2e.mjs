// The dashboard end to end, in a real browser (headless Edge or Chrome through
// playwright-core): run by crates/octopage-server/tests/dashboard_e2e.rs, which starts the
// service with a mock GitHub and passes OCTOPAGE_URL and BROWSER (the browser's executable).
// SCREENSHOTS, if set, is a directory for a screenshot after each step.

import assert from 'node:assert/strict';
import { mkdirSync } from 'node:fs';

import { chromium } from 'playwright-core';

const url = process.env.OCTOPAGE_URL;
const shots = process.env.SCREENSHOTS;
if (shots) mkdirSync(shots, { recursive: true });

const browser = await chromium.launch({
  executablePath: process.env.BROWSER,
  headless: true,
  args: process.platform === 'linux' ? ['--no-sandbox'] : [],
});
const problems = [];
let failed = false;

try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 860 } });
  page.setDefaultTimeout(15_000);
  page.on('console', (m) => {
    // Failed requests are the API's answers (a 401 before signing in, say), checked below.
    if (m.type() === 'error' && !m.text().startsWith('Failed to load resource')) problems.push(m.text());
  });
  page.on('pageerror', (e) => problems.push(e.message));
  page.on('dialog', (d) => d.accept());

  const step = async (name, work) => {
    process.stdout.write(`- ${name}: `);
    await work();
    console.log('ok');
    if (shots) await page.screenshot({ path: `${shots}/${name}.png`, fullPage: true });
  };
  const editor = async (text) => {
    await page.locator('.cm-content').click();
    await page.keyboard.press('Control+A');
    await page.keyboard.insertText(text);
  };
  const tab = (name) => page.getByRole('navigation', { name: 'Database views' }).getByRole('link', { name });

  await step('sign-in', async () => {
    await page.goto(url);
    await page.getByRole('link', { name: 'Sign in with GitHub' }).click();
    await page.getByTestId('login').waitFor();
    assert.equal(await page.getByTestId('login').textContent(), 'ada');
  });

  await step('create-database', async () => {
    await page.getByRole('heading', { name: 'Add a database' }).waitFor();
    assert.equal(await page.getByRole('combobox', { name: 'Repository' }).inputValue(), 'ada/db');
    await page.getByLabel('Not encrypted').check();
    await page.getByRole('button', { name: 'Create database' }).click();
    await page.waitForURL(/\/databases\/db_[0-9a-f]+$/);
    await page.getByRole('heading', { name: /ada\/db/ }).waitFor();
  });

  await step('console', async () => {
    await editor("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);\nINSERT INTO notes(body) VALUES('hello'), ('world');");
    await page.getByTestId('run').click();
    await page.getByTestId('run-summary').filter({ hasText: '2 statements' }).waitFor();
    assert.match(await page.getByTestId('run-summary').textContent(), /committed [0-9a-f]{10}/);
    await editor('SELECT id, body FROM notes ORDER BY id');
    await page.keyboard.press('Control+Enter');
    await page.getByRole('cell', { name: 'world' }).waitFor();
    assert.match(await page.getByTestId('run-summary').textContent(), /2 rows/);
    await editor('SELECT * FROM missing');
    await page.getByTestId('run').click();
    await page.getByRole('alert').filter({ hasText: 'no such table' }).waitFor();
  });

  await step('tables', async () => {
    await tab('Tables').click();
    await page.getByRole('button', { name: 'notes' }).waitFor();
    await page.getByText('2 rows').waitFor();
    await page.getByRole('cell', { name: 'hello' }).waitFor();
  });

  await step('history', async () => {
    await tab('History').click();
    await page.getByTestId('commits').getByText("INSERT INTO notes(body) VALUES('hello')").waitFor();
    // The oldest commit made the empty database: as of then, no tables.
    await page.getByRole('link', { name: 'Browse as of this commit' }).last().click();
    await page.getByTestId('as-of').waitFor();
    await page.getByText('No tables then.').waitFor();
    await page.getByRole('button', { name: 'Back to now' }).click();
    await page.getByRole('button', { name: 'notes' }).waitFor();
  });

  await step('branches', async () => {
    await tab('Branches').click();
    await page.getByLabel('New branch from main').fill('feature');
    await page.getByRole('button', { name: 'Create branch' }).click();
    await page.getByRole('cell', { name: 'feature' }).waitFor();
    await page.getByRole('button', { name: 'Merge into main' }).click();
    await page.getByText('Nothing to merge from feature.').waitFor();
  });

  await step('settings', async () => {
    await tab('Size and settings').click();
    await page.getByRole('meter', { name: 'Repository' }).waitFor({ timeout: 60_000 });
    await page.getByRole('meter', { name: 'Live data' }).waitFor();
    await page.getByRole('combobox', { name: 'Keep' }).selectOption('keep_count');
    await page.getByRole('spinbutton', { name: 'N', exact: true }).fill('50');
    await page.getByRole('button', { name: 'Save settings' }).click();
    await page.getByText('Saved, as a commit beside the data.').waitFor();
    await page.getByRole('button', { name: 'Install the maintenance workflow' }).click();
    await page.getByText(/Installed at/).waitFor();
  });

  await step('settings-dark', async () => {
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.getByRole('meter', { name: 'Repository' }).waitFor();
  });
  await page.emulateMedia({ colorScheme: 'light' });

  await step('keys', async () => {
    await page.getByRole('link', { name: 'API keys' }).click();
    await page.getByLabel('New key named').fill('e2e key');
    await page.getByRole('button', { name: 'Create key' }).click();
    assert.match(await page.getByTestId('secret').textContent(), /^opk_[0-9a-f]{40}$/);
    await page.getByRole('button', { name: 'Done' }).click();
    const row = page.getByRole('row', { name: /e2e key/ });
    await row.waitFor();
    await row.getByRole('button', { name: 'Revoke' }).click();
    await row.waitFor({ state: 'detached' });
  });

  await step('usage-chart', async () => {
    await page.getByRole('link', { name: 'Usage' }).click();
    await page.getByRole('heading', { name: 'ada' }).waitFor();
    await page.getByRole('img', { name: /Requests a day/ }).waitFor();
    // Today's column: its value on hover (and on focus, for the keyboard).
    await page.locator('rect.hit').last().hover();
    await page.locator('.tooltip').filter({ hasText: 'requests' }).waitFor();
    assert.ok(Number((await page.locator('.tooltip strong').textContent()).replace(/\D/g, '')) > 0);
  });

  await step('usage', async () => {
    await page.getByRole('button', { name: 'Last 7 days' }).click();
    await page.getByRole('button', { name: 'Show table' }).click();
    assert.equal(await page.locator('table.numbers tbody tr').count(), 7);
  });

  await step('phone', async () => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole('link', { name: 'Databases' }).click();
    await page.getByRole('link', { name: 'ada/db' }).click();
    await page.getByTestId('run').waitFor();
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    assert.ok(overflow <= 0, `the page scrolls sideways by ${overflow}px at phone width`);
  });
  await page.setViewportSize({ width: 1280, height: 860 });

  await step('sign-out', async () => {
    await page.getByRole('button', { name: 'Sign out' }).click();
    await page.getByRole('link', { name: 'Sign in with GitHub' }).waitFor();
    await page.reload();
    await page.getByRole('link', { name: 'Sign in with GitHub' }).waitFor();
  });

  assert.deepEqual(problems, [], 'no script errors or blocked resources');
  console.log('all steps passed');
} catch (error) {
  failed = true;
  console.error(error);
  if (problems.length) console.error('page problems:', problems);
} finally {
  await browser.close();
}
process.exit(failed ? 1 : 0);
