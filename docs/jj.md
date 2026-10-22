# Jujutsu (jj) support

Basic support for [Jujutsu](https://github.com/jj-vcs/jj) checkouts (issue #36).

- **Native** (`src-tauri/src/jj.rs`): a checkout is a jj repo when it or an ancestor has a `.jj`
  folder. It is *colocated* when that folder also has `.git`. Commands run the user's `jj` (found on
  the widened PATH) through `workspace::run_process` from the checkout, with `--no-pager --color=never`,
  a 30 s deadline and bounded output. They use the default `@` working-copy change:
  - `jj_status` → `{ isRepo, colocated, change, files }`, from `jj log -r @ -T <template>` (NUL-separated
    fields: change id, commit id, description, local bookmarks, author, relative time, conflict, empty,
    current) and `jj diff --summary` (renames resolve to their new path). Returns `isRepo: false`
    without running jj when there is no `.jj`.
  - `jj_diff { path? }` → `jj diff --git`, capped at 256 KiB with a `[Diff truncated…` marker. The path
    goes after `--` as a quoted `root-file:"…"` fileset; absolute paths and `..` are refused.
  - `jj_describe { message }`, `jj_new`, `jj_commit { message }` (needs a description) → the new
    status. The text is only ever passed as `--message=<text>`.
  - `jj_log { limit? }` → the last N (1–200, default 20) changes of the repo's default log revset.
  - Mirror tasks never reach jj: `Store::workspace_root` refuses them (`This task runs on <name>.`).
- **Frontend** (`src/jj.ts`): `showJjPanel(container, status, callbacks)` draws the change, files
  (click → `jj_diff` → `openDiff`), Describe/New/Commit and the log, reusing the Changes tab's classes.
  `jjChip(status)` is the label shown in the Git branch card of a colocated repo. Non-colocated repos
  get the panel instead of the Git UI.
- **Preview** (`src/preview-jj.ts`): `invokeJj` mocks the commands. It is off unless
  `window.__previewJj` is `true` (or `'colocated'`), so other suites still see Git.
- **Tests**: `cargo test --lib jj::` parses recorded `jj log`/`jj diff --summary` output and checks that
  user text cannot become an option. `jj::tests::live_round_trip` (ignored) needs `jj` on PATH.
  `tests/jj.spec.ts` mounts the panel against the mock.

Not yet: bookmarks create/move, push/fetch (`jj git push`), rebase/squash, conflict resolution UI, and
`jj workspace` checkouts as Indocode worktrees. The panel needs jj 0.22+ (bookmark template names).

## Wiring

These files are outside the jj worker's scope. Apply them to turn the feature on.

### `src-tauri/src/lib.rs`

Add `mod jj;` after `mod workspace;`. Add these commands before `async fn git_status`:

```rust
#[tauri::command]
async fn jj_status(state: State<'_, AppState>, project_id: String, session_id: Option<String>) -> Result<jj::JjStatus> {
    let root = state.root(&project_id, session_id.as_deref())?;
    blocking(move || jj::status(&root)).await
}
#[tauri::command]
async fn jj_diff(state: State<'_, AppState>, project_id: String, session_id: Option<String>, path: Option<String>) -> Result<String> {
    let root = state.root(&project_id, session_id.as_deref())?;
    blocking(move || jj::diff(&root, path.as_deref())).await
}
#[tauri::command]
async fn jj_describe(state: State<'_, AppState>, project_id: String, session_id: Option<String>, message: String) -> Result<jj::JjStatus> {
    let root = state.root(&project_id, session_id.as_deref())?;
    blocking(move || jj::describe(&root, &message)).await
}
#[tauri::command]
async fn jj_new(state: State<'_, AppState>, project_id: String, session_id: Option<String>) -> Result<jj::JjStatus> {
    let root = state.root(&project_id, session_id.as_deref())?;
    blocking(move || jj::new_change(&root)).await
}
#[tauri::command]
async fn jj_commit(state: State<'_, AppState>, project_id: String, session_id: Option<String>, message: String) -> Result<jj::JjStatus> {
    let root = state.root(&project_id, session_id.as_deref())?;
    blocking(move || jj::commit(&root, &message)).await
}
#[tauri::command]
async fn jj_log(state: State<'_, AppState>, project_id: String, session_id: Option<String>, limit: Option<usize>) -> Result<Vec<jj::Change>> {
    let root = state.root(&project_id, session_id.as_deref())?;
    blocking(move || jj::log(&root, limit.unwrap_or(20))).await
}
```

Register them in the main window's `tauri::generate_handler![…]` right after `git_diff,`:

```rust
                jj_status,
                jj_diff,
                jj_describe,
                jj_new,
                jj_commit,
                jj_log,
```

### `src/main.ts`

```diff
--- src/main.ts	2026-10-22 11:32:52
+++ src/main.ts	2026-10-22 11:46:34
@@ -23,6 +23,7 @@
 import type { AgentEvent, BrowserAttachment, ContextUsage, FileEntry, GitStatus, Message, MessagePage, Mode, Project, Provider, ProviderId, ProviderModel, Session, TaskLink, ToolActivity, TurnChanges } from './types';
 import type { BrowserPanel } from './browser';
 import { call, chooseFolder, preview, subscribe } from './bridge';
+import { jjChip, showJjPanel, type JjStatus } from './jj';
 import { $, button, escape, fileIcon, fileIconsReady, icon, markdown, morph } from './dom';
 import { mountExternalLinks } from './external-links';
 import { bindMessageActions, messageActionsHtml } from './message-actions';
@@ -84,7 +85,7 @@
   projectFilter: localStorage.getItem('indocode.projectFilter') ?? '', collapsedFolders: readCollapsed(localStorage, 'indocode.collapsedFolders'), expandedFolders: new Set<string>(),
   /** Follow-ups typed while an agent works, per task, in delivery order; mirrored from native storage. */
   queues: new Map<string, QueuedMessage[]>(), queueHolds: new Map<string, HoldReason>(), queueEditing: '', queueBusy: new Set<string>(),
-  git: null as GitStatus | null, files: [] as FileEntry[], filePath: '', gitError: '', filesError: '', inspectorLoading: false,
+  git: null as GitStatus | null, jj: null as JjStatus | null, files: [] as FileEntry[], filePath: '', gitError: '', filesError: '', inspectorLoading: false,
   selectedFile: '', selectedContent: '', selectedKind: '' as '' | 'diff' | 'file', selectedStaged: false, filePreviewLoading: false,
   browserUrl: '',
   attachments: [] as string[], drafts: new Map<string, string>(), defaultProvider: (localStorage.getItem('indocode.defaultProvider') ?? localStorage.getItem('indocode.provider') ?? 'claude') as ProviderId,
@@ -2434,15 +2435,16 @@
 async function refreshInspector(): Promise<void> {
   void projectRunner.refresh();
   const request = ++inspectorRequest;
-  const p = project(); if (!p) { state.git = null; state.files = []; state.gitError = ''; state.filesError = ''; state.inspectorLoading = false; renderInspector(); return; }
+  const p = project(); if (!p) { state.git = null; state.jj = null; state.files = []; state.gitError = ''; state.filesError = ''; state.inspectorLoading = false; renderInspector(); return; }
   state.inspectorLoading = true; renderInspector();
   const id = p.id, folder = state.filePath, context = workspaceContext(), key = workspaceKey();
-  const results = await Promise.allSettled([call<GitStatus>('git_status', context), call<FileEntry[]>('list_files', { ...context, path: folder })]);
+  const results = await Promise.allSettled([call<GitStatus>('git_status', context), call<FileEntry[]>('list_files', { ...context, path: folder }), call<JjStatus>('jj_status', context)]);
   if (request !== inspectorRequest || id !== state.projectId || key !== workspaceKey() || folder !== state.filePath) return;
   state.inspectorLoading = false;
   state.git = results[0].status === 'fulfilled' ? results[0].value : null;
   state.gitError = results[0].status === 'rejected' ? errorText(results[0].reason) || 'Git status could not be loaded.' : '';
   state.files = results[1].status === 'fulfilled' ? results[1].value : [];
+  state.jj = results[2].status === 'fulfilled' ? results[2].value : null;
   state.filesError = results[1].status === 'rejected' ? errorText(results[1].reason) || 'The folder could not be read.' : '';
   renderInspector(); renderHeader();
 }
@@ -2602,11 +2604,18 @@
   if (state.inspectorTab === 'files') {
     target.innerHTML = `<div class="file-folder-path"><span>${escape(project()?.name)}${state.filePath ? ' / ' + escape(state.filePath) : ''}</span>${state.filePath ? button('parent-folder', 'Go to parent folder', 'arrow') : ''}</div><div class="file-list">${state.files.map(file => `<div class="file-row"><button data-action="${file.isDir ? 'open-folder' : isPreviewableFile(file.path) ? 'preview-file' : 'open-file'}" data-path="${escape(file.path)}">${fileIcon(file.name, file.isDir)}<span>${escape(file.name)}</span>${file.isDir ? icon('chevron') : ''}</button>${!file.isDir && /\.(html?|svg)$/i.test(file.path) ? button('open-file', 'View source of ' + file.name, 'file', 'file-attach', `data-path="${escape(file.path)}"`) : ''}${!file.isDir ? button('attach-file', 'Attach ' + file.name, 'plus', 'file-attach', `data-path="${escape(file.path)}"`) : ''}</div>`).join('') || '<div class="inspector-empty">This folder is empty.</div>'}</div>`; return;
   }
+  if (state.jj?.isRepo && !state.jj.colocated) {
+    showJjPanel(target, state.jj, {
+      call, notify, context: () => workspaceContext(), changed: status => { state.jj = status; },
+      openDiff: (path, patch) => { selectedFileContext = workspaceContext(); state.selectedFile = path; state.selectedContent = patch; state.selectedKind = 'diff'; state.selectedStaged = false; state.filePreviewLoading = false; state.inspector = true; renderInspector(); },
+    });
+    return;
+  }
   if (!state.git?.isRepo) { target.innerHTML = '<div class="inspector-empty">'+icon('branch')+'<h3>No Git repository</h3><p>Open a Git repository to review changes.</p></div>'; return; }
   const staged = state.git.files.filter(file => ![' ', '?', '!', '.'].includes(file.index));
   const unstaged = state.git.files.filter(file => ![' ', '!', '.'].includes(file.working) || file.index === '?');
   const fileRows = (files: typeof staged, isStaged: boolean) => files.map(file => `<div class="change-row"><button class="change-file" data-action="open-diff" data-path="${escape(file.path)}" data-staged="${isStaged}">${icon('file')}<span><strong>${escape(file.path.split('/').pop())}</strong><small>${escape(file.path.includes('/') ? file.path.slice(0, file.path.lastIndexOf('/')) : 'root')}</small></span><span class="file-status ${file.index === '?' ? 'new' : ''}">${file.index === '?' ? 'A' : escape(isStaged ? file.index : file.working)}</span></button>${button(isStaged ? 'unstage-file' : 'stage-file', isStaged ? 'Unstage file' : 'Stage file', isStaged ? 'close' : 'plus', 'stage-button', `data-path="${escape(file.path)}"`)}</div>`).join('');
-  target.innerHTML = `<div class="branch-card"><span>${icon('branch')}<span>${escape(state.git.branch || 'No commits yet')}</span></span><button data-action="worktree" title="Create isolated worktree" aria-label="Create isolated worktree">${icon('plus')}</button></div><div class="change-summary"><span>${state.git.files.length ? state.git.files.length + ' changed files' : 'Working tree clean'}</span>${state.git.files.length ? '' : icon('check')}</div>${staged.length ? `<section class="change-group"><div class="change-group-title"><span>Staged <small>${staged.length}</small></span><button data-action="unstage-all">Unstage all</button></div>${fileRows(staged, true)}</section>` : ''}${unstaged.length ? `<section class="change-group"><div class="change-group-title"><span>Changes <small>${unstaged.length}</small></span><button data-action="stage-all">Stage all</button></div>${fileRows(unstaged, false)}</section>` : ''}<form id="commit-form" class="commit-form"><label for="commit-message">Commit message</label><textarea id="commit-message" placeholder="Describe these changes…" rows="3" aria-label="Commit message"></textarea><button class="commit-button" ${!staged.length ? 'disabled' : ''}>${icon('check')}Commit${staged.length ? ' ' + staged.length + ' file' + (staged.length > 1 ? 's' : '') : ''}</button></form>`;
+  target.innerHTML = `<div class="branch-card"><span>${icon('branch')}<span>${escape(state.git.branch || 'No commits yet')}</span>${jjChip(state.jj)}</span><button data-action="worktree" title="Create isolated worktree" aria-label="Create isolated worktree">${icon('plus')}</button></div><div class="change-summary"><span>${state.git.files.length ? state.git.files.length + ' changed files' : 'Working tree clean'}</span>${state.git.files.length ? '' : icon('check')}</div>${staged.length ? `<section class="change-group"><div class="change-group-title"><span>Staged <small>${staged.length}</small></span><button data-action="unstage-all">Unstage all</button></div>${fileRows(staged, true)}</section>` : ''}${unstaged.length ? `<section class="change-group"><div class="change-group-title"><span>Changes <small>${unstaged.length}</small></span><button data-action="stage-all">Stage all</button></div>${fileRows(unstaged, false)}</section>` : ''}<form id="commit-form" class="commit-form"><label for="commit-message">Commit message</label><textarea id="commit-message" placeholder="Describe these changes…" rows="3" aria-label="Commit message"></textarea><button class="commit-button" ${!staged.length ? 'disabled' : ''}>${icon('check')}Commit${staged.length ? ' ' + staged.length + ' file' + (staged.length > 1 ? 's' : '') : ''}</button></form>`;
 }
 async function openRunPreview(url: string, context: { projectId: string; sessionId?: string }): Promise<void> {
   const current = () => state.projectId === context.projectId && state.sessionId === (context.sessionId ?? '');
```

### `src/preview.ts`

```diff
--- src/preview.ts	2026-10-22 11:32:52
+++ src/preview.ts	2026-10-22 11:46:34
@@ -568,6 +568,7 @@
 });
 
 export async function invoke<T>(command: string, args: Record<string, any>): Promise<T> {
+  if (command.startsWith('jj_')) return (await import('./preview-jj')).invokeJj<T>(command, args);
   if (command.includes('moodboard')) return (await import('./preview-moodboards')).invokeMoodboards<T>(command, args);
   if (command === 'bots_list' || command.startsWith('bot_')) return invokeBots<T>(command, args, {
     invoke, emit,
```

### `playwright.config.ts` (and `playwright.webkit.config.ts`)

Add `'**/jj.spec.ts'` to `testMatch`.

### `styles.css` (optional)

```css
.jj-chip, .jj-bookmark { font-size: 11px; padding: 1px 6px; border-radius: 999px; background: rgb(var(--accent-rgb) / .14); color: var(--accent-ink); white-space: nowrap; }
.jj-chip { margin-left: 6px; }
.jj-actions { display: flex; gap: 6px; }
.jj-actions .commit-button { flex: 1; }
.jj-log-row.current strong::before { content: '@ '; color: var(--accent); }
```
