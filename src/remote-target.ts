import { escape, icon } from './dom';
import type { RemoteRepoCheck, RemoteTarget } from './remote-types';
import type { Session } from './types';

/* The context bar's computer choice (Indocode Connect). A new task can start on another
 * signed-in computer; a task that runs there shows that computer, locked. */

export const LOCAL = 'local';

/** The control beside Local / Worktree; empty when there is nothing to choose. */
export function computerControl(session: Session, fresh: boolean, targets: RemoteTarget[], chosen: string): string {
  if (session.remoteDevice) {
    const name = escape(session.remoteDevice.name);
    return `<span class="composer-computer composer-computer-fixed" aria-label="Computer: ${name} (fixed for this task)" title="This task runs on ${name}. Start a new task to use another computer.">${icon('monitor')}<span>on ${name}</span></span>`;
  }
  if (!fresh || !targets.length) return '';
  const name = targets.find(target => target.deviceId === chosen)?.name ?? 'This computer';
  return `<button type="button" id="composer-computer" class="composer-computer" data-action="computer-menu" aria-label="Computer: ${escape(name)}" aria-haspopup="dialog" aria-expanded="false" title="Where this task runs">${icon('monitor')}<span>${escape(name)}</span>${icon('down')}</button>`;
}

export function computerItems(targets: RemoteTarget[], chosen: string) {
  return [
    { id: LOCAL, label: 'This computer', icon: 'monitor', selected: chosen === LOCAL },
    ...targets.map(target => ({ id: target.deviceId, label: target.name, description: target.online ? target.platform : 'Offline', icon: 'monitor', selected: target.deviceId === chosen, disabled: !target.online })),
  ];
}

/** Why GitHub is behind this computer, as one sentence, or '' when it isn't. */
export function behindText(check: RemoteRepoCheck): string {
  const reasons = [
    check.ahead > 0 ? `${check.ahead} unpushed ${check.ahead === 1 ? 'commit' : 'commits'}` : '',
    check.dirty ? 'uncommitted changes' : '',
    check.pushed ? '' : `branch ${check.branch} not pushed`,
  ].filter(Boolean);
  return reasons.length ? `GitHub is behind this computer: ${reasons.join(', ')}.` : '';
}

export function behindDialog(name: string, check: RemoteRepoCheck): string {
  return `<form id="remote-behind-form"><p class="dialog-description">${escape(behindText(check))} ${escape(name)} will start from GitHub’s ${escape(check.branch)}.</p><div class="dialog-footer"><button type="button" class="secondary-button" data-action="close-dialog">Cancel</button><button type="submit" class="primary-button">Continue</button></div></form>`;
}
