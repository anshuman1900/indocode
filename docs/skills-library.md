# Skills library and rules registry

Issues #15 (skills) and #45 (rules). This is a small first version. It lets you import a skill or rule once, turn it on for each harness and scope, and see what each installed harness actually has.

## Storage

Everything is files under the app data folder. There is no schema migration.

```
<app data>/skills/<id>/SKILL.md     skill (the folder keeps its other files)
<app data>/skills/<id>/RULE.md      rule (one Markdown file)
<app data>/skills/<id>.json         Meta: source (folder path or owner/name/path, ref, commit),
                                    content SHA-256, acknowledged hash, activations
```

Each activation records the harness, scope, project checkout, surface, the library hash it wrote, the SHA-256 of every file or block it wrote, and any `.git/info/exclude` lines it added.

## Adapters (`src-tauri/src/skills/adapter.rs`)

| Harness | Skill, user | Skill, project | Rule, user | Rule, project |
|---|---|---|---|---|
| Claude Code | `~/.claude/skills/<id>/` | `<checkout>/.claude/skills/<id>/` | block in `~/.claude/CLAUDE.md` | block in `<checkout>/CLAUDE.md` |
| Codex | `~/.codex/skills/<id>/` | `<checkout>/.codex/skills/<id>/` | block in `~/.codex/AGENTS.md` | block in `<checkout>/AGENTS.md` |
| Others | `prompt` | `prompt` | `prompt` | `prompt` |

- A rule block is wrapped in `<!-- indocode:rule:<id> -->` … `<!-- /indocode:rule:<id> -->`. Text outside the block is never changed.
- Indocode never writes into a skill folder it did not create, never overwrites a file or block whose hash differs from what it wrote, and on deactivation leaves edited files in place (`Change.kept`).
- In a Git checkout, project scope adds the skill folder (and a rule file, but only when Indocode created that file) to `info/exclude`. The path comes from `git rev-parse --git-path info/exclude`, so worktrees work too. Passing `sessionId` scopes the activation to that task's worktree.
- Harnesses marked `prompt` only record the activation. Nothing injects these items into prompts yet (see Not yet).

## Matrix and safety

- `skills_matrix` lists each installed file harness (its CLI is on PATH or `~/.<harness>` exists), plus any prompt harness in use. Each cell is `active`, `outdated` (a newer library version is not activated yet), `drifted`, `missing`, `prompt` or `off`. It also lists skill folders Indocode does not manage, unknown Indocode blocks, and instruction files with text Indocode does not manage.
- Importing again updates the library only. Harnesses keep the old version and show `outdated` until you activate again.
- The safety scan (`skills/safety.rs`) flags secret-shaped tokens and named secrets, instructions that turn off approvals or the sandbox, and downloads piped into a shell. If an item has findings, activation fails until the user calls it with `acknowledge: true`. That acknowledgement is stored per content hash.
- GitHub import uses `gh api` through `github::run_gh` (`workspace::run_process`). Limits: 64 files and 1 MB.

## Commands

`skills_list()`, `skills_import_folder({ path, kind })`, `skills_import_github({ repo, path, gitRef?, kind })`, `skills_activate({ id, harness, scope, projectId?, sessionId?, acknowledge? })`, `skills_deactivate({ id, harness, scope, projectId?, sessionId? })`, `skills_matrix({ projectId?, sessionId? })`, `skills_scan({ id })`, `skills_remove({ id })`. Here `kind` is `skill` or `rule`, and `scope` is `user` or `project`. Activate and deactivate return `{ items, kept }`.

## Not yet

Prompt injection for prompt harnesses (that belongs with the token saver's send-time block), role and task scopes beyond a session worktree, version diffs with promote/skip/pin, folding the Token saver packs into the library, GitLab, conflict detection between rules.

## Wiring

These hunks fall outside the module's files.

`src-tauri/src/lib.rs`, module list (next to `mod token_saver;`):

```rust
mod skills;
```

`src-tauri/src/lib.rs`, `generate_handler!` (after `token_saver::token_saver_install,`):

```rust
                skills::skills_list,
                skills::skills_import_folder,
                skills::skills_import_github,
                skills::skills_activate,
                skills::skills_deactivate,
                skills::skills_matrix,
                skills::skills_scan,
                skills::skills_remove,
```

`src/settings.ts`:

```ts
import { mountSkillsLibrary, type SkillsLibraryHost } from './skills-library';
// SettingsSection union: add 'skills'
export type SettingsSection = 'general' | 'appearance' | 'agents' | 'providers' | 'teams' | 'tools' | 'skills' | 'cloud' | 'remote' | 'github' | 'about';
// SettingsOptions:
  /** Skills library and rules registry. */
  skills?: SkillsLibraryHost;
// SECTIONS, after 'tools':
  { id: 'skills', label: 'Skills & rules', icon: 'widget', description: 'Import skills and rules once, turn them on per harness, and check for drift.' },
// sectionHtml():
    case 'skills': return '<div id="skills-library"></div>';
// mountSection():
    if (id === 'skills' && options.skills) cleanups.push(mountSkillsLibrary(panel.querySelector<HTMLElement>('#skills-library')!, options.skills));
```

`src/main.ts`, in the `mountSettings` options next to `cloud: { call, … }`:

```ts
      skills: { call, projects: () => userProjects().map(p => ({ id: p.id, name: p.name })) },
```

`src/preview.ts`:

```ts
import { createPreviewSkills } from './preview-skills';
const previewSkills = createPreviewSkills();
// in invoke()'s switch, before `default:`
    case 'skills_list': case 'skills_import_folder': case 'skills_import_github': case 'skills_activate':
    case 'skills_deactivate': case 'skills_matrix': case 'skills_scan': case 'skills_remove':
      result = previewSkills.call(command, args); break;
```

`playwright.config.ts`, `testMatch`: add `'**/skills-library.spec.ts'`. The spec mounts the section on its own page, so it passes before or after the Settings wiring.
