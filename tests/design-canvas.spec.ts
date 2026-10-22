import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';

// The Design canvas module under the app's Content Security Policy, mounted on its own
// with the preview mock as its host: artboards render in script-free sandboxed iframes,
// clicking an element pins a comment, comments save and become an agent prompt, and an
// agent write reloads the artboard.
const errors = new WeakMap<Page, string[]>();
test.afterEach(async ({ page }) => { expect(errors.get(page) ?? []).toEqual([]); });

async function mount(page: Page): Promise<void> {
  const runtime: string[] = []; errors.set(page, runtime);
  page.on('pageerror', error => runtime.push(error.message));
  page.on('console', message => { if (/Content Security Policy/i.test(message.text())) runtime.push(message.text()); });
  const csp = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8')).app.security.csp.replace("connect-src ipc: http://ipc.localhost", "connect-src 'self' ws: ipc: http://ipc.localhost");
  await page.route(url => new URL(url).pathname === '/', async route => {
    const response = await route.fetch();
    await route.fulfill({ response, body: (await response.text()).replace('<head>', `<head><meta http-equiv="Content-Security-Policy" content="${csp}">`) });
  });
  await page.goto('/');
  await expect(page.locator('#prompt')).toBeVisible();
  await page.evaluate(async () => {
    const { mountDesignCanvas } = await import('/src/design-canvas.ts' as string);
    const { createPreviewDesign } = await import('/src/preview-design.ts' as string);
    const mock = createPreviewDesign();
    const w = window as any;
    w.__design = { mock, calls: [] as string[], composed: '' };
    document.body.innerHTML = '<section id="design-panel" class="design-panel" aria-label="Design" style="position:fixed;inset:0;background:var(--bg,#fff)"></section>';
    const call = async (command: string, args: any) => {
      w.__design.calls.push(command);
      if (command === 'design_list') return mock.list();
      if (command === 'design_read') return mock.read(args.name);
      if (command === 'design_comments') return mock.comments();
      if (command === 'design_save_comments') return mock.saveComments(args.comments);
      throw new Error(`Unexpected ${command}`);
    };
    w.__design.panel = mountDesignCanvas(document.querySelector('#design-panel'), {
      call, notify: (text: string) => { throw new Error(text); },
      context: () => ({ projectId: 'project-1', sessionId: 'session-1' }),
      compose: (prompt: string) => { w.__design.composed = prompt; },
    });
    w.__design.panel.show();
  });
}
const panel = (page: Page) => page.getByRole('region', { name: 'Design', exact: true });
const frame = (page: Page, name: string) => page.frameLocator(`iframe[title="${name}"]`);

test('renders artboards in sandboxed, script-free frames at their widths', async ({ page }) => {
  await mount(page);
  await expect(frame(page, 'home.html').getByRole('heading', { name: 'Ship calmer releases' })).toBeVisible();
  await expect(frame(page, 'phone.html').getByText('Deploys')).toBeVisible();
  const frames = page.locator('.design-board iframe');
  await expect(frames).toHaveCount(2);
  for (const f of await frames.all()) await expect(f).toHaveAttribute('sandbox', 'allow-same-origin');
  expect(await page.locator('.design-board').nth(1).evaluate(el => el.style.width)).toBe('390px');
  // The sample comment is pinned on its element.
  await expect(page.locator('.design-mark')).toHaveCount(1);
  await expect(panel(page).getByRole('button', { name: 'Send 1 comment to agent' })).toBeEnabled();
});

test('comments on a clicked element, saves it and sends the prompt', async ({ page }) => {
  await mount(page);
  const title = frame(page, 'home.html').getByRole('heading', { name: 'Ship calmer releases' });
  await expect(title).toBeVisible();
  await panel(page).getByRole('button', { name: 'Comment on an element' }).click();
  const box = (await page.locator('iframe[title="home.html"]').evaluate((f: HTMLIFrameElement) => {
    const r = f.getBoundingClientRect(), t = f.contentDocument!.querySelector('h1')!.getBoundingClientRect(), z = r.width / f.offsetWidth;
    return { x: r.x + (t.x + t.width / 2) * z, y: r.y + (t.y + t.height / 2) * z };
  }));
  await page.mouse.click(box.x, box.y);
  const field = panel(page).getByRole('textbox', { name: 'Comment on #hero-title' });
  await expect(field).toBeFocused();
  await field.fill('Make the title shorter');
  await panel(page).getByRole('button', { name: 'Add comment' }).click();
  await expect(panel(page).getByText('Make the title shorter')).toBeVisible();
  const saved = await page.evaluate(() => (window as any).__design.mock.comments());
  expect(saved).toHaveLength(2);
  expect(saved[1]).toMatchObject({ artboard: 'home.html', selector: '#hero-title', status: 'open', text: 'Make the title shorter' });
  expect(saved[1].bbox.width).toBeGreaterThan(100);
  await panel(page).getByRole('button', { name: 'Send 2 comments to agent' }).click();
  const prompt = await page.evaluate(() => (window as any).__design.composed);
  expect(prompt).toContain('indocode_design_resolve');
  expect(prompt).toContain('`#hero-title`');
  expect(prompt).toContain('Make the title shorter');
  // Resolving drops it from the prompt and the canvas marks.
  await panel(page).getByRole('button', { name: 'Resolve comment' }).first().click();
  await expect(page.locator('.design-mark')).toHaveCount(1);
  await expect(panel(page).getByRole('button', { name: 'Send 1 comment to agent' })).toBeEnabled();
});

test('reloads an artboard the agent rewrote, and pans and zooms', async ({ page }) => {
  await mount(page);
  await expect(frame(page, 'home.html').getByText('Ship calmer releases')).toBeVisible();
  await page.evaluate(() => { const d = (window as any).__design; d.mock.write('home.html', '<h1>Revised</h1>'); d.mock.write('new.html', '<p>Fresh</p>'); return d.panel.refresh(); });
  await expect(frame(page, 'home.html').getByText('Revised')).toBeVisible();
  await expect(frame(page, 'new.html').getByText('Fresh')).toBeVisible();
  const zoom = panel(page).getByRole('button', { name: 'Fit artboards' });
  const before = await zoom.textContent();
  await panel(page).getByRole('button', { name: 'Zoom in' }).click();
  await expect(zoom).not.toHaveText(before!);
  const world = page.locator('.design-world');
  const start = await world.evaluate(el => el.style.transform);
  await page.mouse.move(700, 600); await page.mouse.down(); await page.mouse.move(600, 500, { steps: 4 }); await page.mouse.up();
  expect(await world.evaluate(el => el.style.transform)).not.toBe(start);
  // A hidden canvas makes no calls until it is shown again.
  const calls = await page.evaluate(() => { const d = (window as any).__design; d.panel.hide(); d.panel.refresh(); return d.calls.length; });
  expect(await page.evaluate(() => (window as any).__design.calls.length)).toBe(calls);
});
