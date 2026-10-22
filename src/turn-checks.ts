//! Optional turn-end project checks: Settings → Agents turns them on, and a
//! failing check queues one fix-it message through the ordinary message
//! queue, where the user sees and can edit or cancel it. One repair per run:
//! a repair turn's own failure waits for the user instead of looping.

const KEY = 'indocode.turnChecks';
const repaired = new Set<string>();
/** Manual sends per task, so a check that started before one can tell it is stale. */
const manualSends = new Map<string, number>();

export function enabled(): boolean {
  try { return localStorage.getItem(KEY) === 'true'; } catch { return false; }
}

export function setEnabled(on: boolean): void {
  try { localStorage.setItem(KEY, String(on)); } catch { /* Kept for this window only. */ }
}

/** A message the user sent themselves restarts the repair budget for that task. */
export function noteManualSend(sessionId: string): void {
  repaired.delete(sessionId);
  manualSends.set(sessionId, (manualSends.get(sessionId) ?? 0) + 1);
}

export interface CheckReport {
  checked: boolean;
  name?: string;
  ok?: boolean;
  output?: string;
  reason?: string;
}

/** Pure decision: checks on, the check ran, it failed, and this run has no repair yet. */
export function shouldRepair(input: { on: boolean; report: CheckReport; alreadyRepaired: boolean }): boolean {
  return input.on && input.report.checked === true && input.report.ok === false && !input.alreadyRepaired;
}

/** The queued repair text; the failure output is already bounded natively. */
export function repairPrompt(report: CheckReport): string {
  const name = report.name ?? 'project checks';
  const output = (report.output ?? '').trim();
  return `Project checks failed after your last turn (${name}). Fix the failures before anything else; do not start new work.${output ? `\n\n\`\`\`\n${output}\n\`\`\`` : ''}`;
}

/** A value read now, or a getter read again after the check finishes. */
type Live<T> = T | (() => T);
const read = <T>(value: Live<T>): T => (typeof value === 'function' ? (value as () => T)() : value);

export interface ChecksContext {
  call: <T>(command: string, args?: Record<string, unknown>) => Promise<T>;
  /** The finished task, when it is an ordinary project task. */
  task: { id: string } | undefined;
  isTeam: boolean;
  isBot: boolean;
  /** Pass getters: they are read again after the check, which can take two minutes. */
  isRunning: Live<boolean>;
  queued: Live<number>;
  /** Permission mode; plan and read-only tasks are never checked. */
  mode?: Live<string | undefined>;
  /** Called after the repair joins the queue, so the idle task starts on it. */
  onQueued(sessionId: string): void;
  onError(error: unknown): void;
}

const READ_ONLY = new Set(['plan', 'read-only', 'readonly']);

function idle(context: ChecksContext): boolean {
  return !read(context.isRunning) && read(context.queued) === 0 && !READ_ONLY.has(read(context.mode) ?? '');
}

/** Runs the checkout's fast check after a clean turn and queues one repair on failure. */
export async function afterRun(sessionId: string, context: ChecksContext): Promise<void> {
  if (!enabled() || !context.task || context.isTeam || context.isBot || !idle(context)) return;
  const sends = manualSends.get(sessionId) ?? 0;
  const stale = () => !enabled() || !idle(context) || (manualSends.get(sessionId) ?? 0) !== sends;
  let report: CheckReport;
  try {
    report = await context.call<CheckReport>('run_turn_checks', { sessionId });
  } catch (error) {
    context.onError(error);
    return;
  }
  if (stale()) return; // The task moved on while the check ran: drop the late result.
  if (report.checked && report.ok) repaired.delete(sessionId);
  if (!shouldRepair({ on: enabled(), report, alreadyRepaired: repaired.has(sessionId) })) return;
  repaired.add(sessionId);
  try {
    await context.call('queue_add', { sessionId, text: repairPrompt(report), attachments: [] });
    context.onQueued(sessionId);
  } catch (error) {
    repaired.delete(sessionId);
    context.onError(error);
  }
}
