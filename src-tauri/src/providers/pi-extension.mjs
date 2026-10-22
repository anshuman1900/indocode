// Indocode app tools that only read state stay available in Plan mode; every other app tool is blocked.
const PLAN_APP_TOOLS = new Set([
  'indocode_title', 'indocode_team_directory', 'indocode_read_team_mailbox',
  'indocode_list_tasks', 'indocode_read_task', 'indocode_search_tasks', 'indocode_list_projects',
  'indocode_list_workers', 'indocode_read_worker', 'indocode_list_subagents', 'indocode_list_queue',
  'indocode_list_local_servers', 'indocode_list_moodboards', 'indocode_read_moodboard', 'indocode_read_moodboard_image',
  'indocode_vm_list', 'indocode_vm_screenshot', 'indocode_simulator_devices', 'indocode_simulator_describe',
  'indocode_simulator_screenshot', 'indocode_jev_read', 'indocode_jev_list', 'indocode_jev_screenshot',
  'indocode_jev_assessments', 'indocode_bot_recall', 'indocode_bot_list_schedules', 'indocode_code_search',
]);

export default async function (pi) {
  const config = JSON.parse(process.env.INDOCODE_PI_BRIDGE);
  // The bearer token must not reach the commands Pi runs (they inherit this environment).
  delete process.env.INDOCODE_PI_BRIDGE;
  const request = async (method, params, signal) => {
    const response = await fetch(config.url, {
      method: 'POST',
      signal,
      headers: { 'Content-Type': 'application/json', Authorization: config.authorization },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
    });
    if (!response.ok) throw new Error('Indocode tools connection failed.');
    const result = await response.json();
    if (result.error) throw new Error(result.error.message);
    return result.result;
  };
  const tools = config.url ? (await request('tools/list', {})).tools : [];
  const names = new Set(tools.map(tool => tool.name));
  for (const tool of tools) {
    pi.registerTool({
      name: tool.name,
      label: tool.name,
      // omp hides extension tools behind discovery unless they are essential; Pi ignores this.
      loadMode: 'essential',
      description: tool.description,
      parameters: tool.inputSchema,
      async execute(_id, args, signal) {
        const result = await request('tools/call', { name: tool.name, arguments: args }, signal);
        if (result.isError) throw new Error(result.content.filter(item => item.type === 'text').map(item => item.text).join('\n'));
        return result;
      },
    });
  }
  pi.on('session_start', (_event, ctx) => ctx.ui.setStatus('indocode-bridge', 'ready'));
  pi.on('tool_call', async (event, ctx) => {
    const app = names.has(event.toolName);
    if (['read', 'grep', 'find', 'ls'].includes(event.toolName) || (app && PLAN_APP_TOOLS.has(event.toolName))) return;
    if (config.mode === 'plan') return { block: true, reason: 'This tool is unavailable in Plan mode.' };
    if (app) return;
    if (config.mode === 'full') return;
    if (config.mode === 'auto' && ['edit', 'write'].includes(event.toolName)) return;
    try {
      if (await ctx.ui.confirm(`Allow ${event.toolName}?`, JSON.stringify(event.input).slice(0, 4000))) return;
    } catch {}
    return { block: true, reason: 'Declined by the user.' };
  });
}
