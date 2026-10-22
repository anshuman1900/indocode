import test from 'node:test';
import assert from 'node:assert/strict';

const memory = new Map();
globalThis.localStorage = { getItem: key => memory.get(key) ?? null, setItem: (key, value) => memory.set(key, String(value)), removeItem: key => memory.delete(key) };
const checks = await import('../src/turn-checks.ts');

const failed = { checked: true, name: 'npm test', ok: false, output: '3 tests failed' };
const passed = { checked: true, name: 'npm test', ok: true, output: '' };
const noPlan = { checked: false, reason: 'no fast check' };

test('the toggle stays off until the user turns it on', () => {
  memory.clear();
  assert.equal(checks.enabled(), false);
  checks.setEnabled(true);
  assert.equal(checks.enabled(), true);
  checks.setEnabled(false);
  assert.equal(checks.enabled(), false);
});

test('one repair per run: a queued repair, then silence until a manual send', async () => {
  memory.clear();
  memory.set('indocode.turnChecks', 'true');
  const calls = [];
  const queued = [];
  const context = id => ({
    call: async (command, args) => { calls.push(command); return command === 'queue_add' ? { id: 'entry' } : (id === 'broken' ? failed : passed); },
    task: { id },
    isTeam: false,
    isBot: false,
    isRunning: false,
    queued: 0,
    onQueued: queuedId => queued.push(queuedId),
    onError: error => { throw error; },
  });

  // A failing check queues exactly one repair and starts the task on it.
  await checks.afterRun('broken', context('broken'));
  assert.deepEqual(calls, ['run_turn_checks', 'queue_add']);
  assert.deepEqual(queued, ['broken']);

  // The repair turn fails its own check: no second repair, no loop.
  calls.length = 0; queued.length = 0;
  await checks.afterRun('broken', context('broken'));
  assert.deepEqual(calls, ['run_turn_checks']);
  assert.deepEqual(queued, []);

  // A manual send restarts the budget; the next failure repairs again.
  checks.noteManualSend('broken');
  calls.length = 0; queued.length = 0;
  await checks.afterRun('broken', context('broken'));
  assert.deepEqual(calls, ['run_turn_checks', 'queue_add']);

  // A passing check also clears the budget for the next run.
  calls.length = 0;
  await checks.afterRun('fine', context('fine'));
  assert.deepEqual(calls, ['run_turn_checks']);
  checks.noteManualSend('fine');
  calls.length = 0;
  await checks.afterRun('fine', context('fine'));
  assert.deepEqual(calls, ['run_turn_checks']);
});

test('guards: off, teams, bots, running tasks and occupied queues never run checks', async () => {
  memory.clear();
  memory.set('indocode.turnChecks', 'true');
  const base = { call: async () => failed, task: { id: 't' }, isTeam: false, isBot: false, isRunning: false, queued: 0, onQueued: () => {}, onError: () => { throw new Error('unexpected'); } };
  const seen = [];
  await checks.afterRun('t', { ...base, call: async command => { if (command === 'run_turn_checks') seen.push('run'); return failed; } });
  assert.deepEqual(seen, ['run']);

  for (const broken of [
    { ...base, task: undefined },
    { ...base, isTeam: true },
    { ...base, isBot: true },
    { ...base, isRunning: true },
    { ...base, queued: 2 },
  ]) {
    seen.length = 0;
    await checks.afterRun('t', broken);
    assert.deepEqual(seen, [], JSON.stringify(broken));
  }

  memory.set('indocode.turnChecks', 'false');
  seen.length = 0;
  await checks.afterRun('t', base);
  assert.deepEqual(seen, [], 'the toggle off skips everything');
});

test('the repair prompt names the check and carries the native output tail', () => {
  const prompt = checks.repairPrompt(failed);
  assert.match(prompt, /Project checks failed after your last turn \(npm test\)/);
  assert.match(prompt, /```\n3 tests failed\n```/);
  assert.equal(checks.repairPrompt({ checked: true, name: 'cargo check', ok: false, output: '' }), 'Project checks failed after your last turn (cargo check). Fix the failures before anything else; do not start new work.');
  assert.equal(checks.repairPrompt(noPlan), 'Project checks failed after your last turn (project checks). Fix the failures before anything else; do not start new work.');
});

test('shouldRepair is the pure gate behind the flow', () => {
  assert.equal(checks.shouldRepair({ on: true, report: failed, alreadyRepaired: false }), true);
  assert.equal(checks.shouldRepair({ on: false, report: failed, alreadyRepaired: false }), false);
  assert.equal(checks.shouldRepair({ on: true, report: failed, alreadyRepaired: true }), false);
  assert.equal(checks.shouldRepair({ on: true, report: passed, alreadyRepaired: false }), false);
  assert.equal(checks.shouldRepair({ on: true, report: noPlan, alreadyRepaired: false }), false);
});

test('late results are dropped when the task moved on during the check', async () => {
  memory.clear();
  memory.set('indocode.turnChecks', 'true');
  for (const [label, change] of [
    ['running', state => { state.running = true; }],
    ['queued', state => { state.queued = 1; }],
    ['setting off', () => memory.set('indocode.turnChecks', 'false')],
    ['manual send', () => checks.noteManualSend('late')],
  ]) {
    memory.set('indocode.turnChecks', 'true');
    const state = { running: false, queued: 0 };
    const calls = [];
    await checks.afterRun('late', {
      call: async command => { calls.push(command); if (command === 'run_turn_checks') { change(state); return failed; } return {}; },
      task: { id: 'late' }, isTeam: false, isBot: false,
      isRunning: () => state.running, queued: () => state.queued,
      onQueued: () => assert.fail(label), onError: error => { throw error; },
    });
    assert.deepEqual(calls, ['run_turn_checks'], label);
  }
});

test('plan and read-only tasks are never checked, including a mode change mid-check', async () => {
  memory.clear();
  memory.set('indocode.turnChecks', 'true');
  const base = { task: { id: 'p' }, isTeam: false, isBot: false, isRunning: false, queued: 0, onQueued: () => {}, onError: error => { throw error; } };
  for (const mode of ['plan', 'read-only']) {
    const calls = [];
    await checks.afterRun('p', { ...base, mode, call: async command => { calls.push(command); return failed; } });
    assert.deepEqual(calls, [], mode);
  }
  let mode = 'auto';
  const calls = [];
  await checks.afterRun('p', { ...base, mode: () => mode, call: async command => { calls.push(command); mode = 'plan'; return failed; } });
  assert.deepEqual(calls, ['run_turn_checks']);
});
