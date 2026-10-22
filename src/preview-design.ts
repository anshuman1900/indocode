import type { Artboard, DesignComment } from './design-canvas';

/* Browser-preview stand-ins for the design_* commands: two artboards and one comment. */

const HOME = `<!doctype html><html><head><style>
body{margin:0;font:16px system-ui,sans-serif;color:#1d1d1f}
header{display:flex;justify-content:space-between;padding:20px 40px;border-bottom:1px solid #eee}
main{padding:80px 40px}h1{font-size:44px;margin:0 0 16px}
.cta{display:inline-block;padding:12px 20px;border-radius:8px;background:#1d1d1f;color:#fff}
</style></head><body><header><strong>Orbit</strong><nav>Pricing · Docs</nav></header>
<main><h1 id="hero-title">Ship calmer releases</h1><p>Orbit watches every deploy for you.</p><a class="cta">Start free</a></main></body></html>`;
const PHONE = `<!doctype html><html><head><meta name="artboard-width" content="390"><style>
body{margin:0;font:15px system-ui,sans-serif}section{padding:24px}h2{margin:0 0 8px}li{padding:10px 0;border-bottom:1px solid #eee}
</style></head><body><section><h2>Deploys</h2><ul><li>api · 2 min ago</li><li>web · 1 h ago</li></ul></section></body></html>`;

export function createPreviewDesign() {
  const files = new Map<string, { html: string; modified: number }>([['home.html', { html: HOME, modified: 1 }], ['phone.html', { html: PHONE, modified: 1 }]]);
  let comments: DesignComment[] = [{ id: 'sample-1', artboard: 'home.html', selector: 'body > main > a', bbox: { x: 40, y: 230, width: 108, height: 44 }, text: 'Use the accent color here.', status: 'open', createdAt: Date.now() - 60_000 }];
  let clock = 1;
  const list = (): Artboard[] => [...files].sort(([a], [b]) => a.localeCompare(b)).map(([name, f]) => ({ name, path: `.indocode/design/${name}`, size: f.html.length, modified: f.modified }));
  return {
    list,
    read(name: string): string { const file = files.get(name); if (!file) throw new Error(`No artboard named ${name}.`); return file.html; },
    comments: (): DesignComment[] => structuredClone(comments),
    saveComments(next: DesignComment[]): null { comments = structuredClone(next); return null; },
    /** What indocode_design_write does; preview callers emit `design-changed` after. */
    write(name: string, html: string): Artboard { files.set(name, { html, modified: ++clock }); return list().find(a => a.name === name)!; },
    resolve(id: string, note?: string): void { comments = comments.map(c => c.id === id ? { ...c, status: 'resolved', note } : c); },
  };
}
