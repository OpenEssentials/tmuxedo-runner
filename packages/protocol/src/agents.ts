/**
 * The agents tmuxedo recognises: the `kind` a pane's agent carries (an agent
 * file id in runner/crates/detect/agents) and the name people know it by.
 * test/agents.test.ts checks this table against those files.
 */
export const AGENT_TITLES: Readonly<Record<string, string>> = {
  claude: 'Claude Code',
  codex: 'Codex CLI',
  gemini: 'Gemini CLI',
  cursor: 'Cursor CLI',
  copilot: 'GitHub Copilot CLI',
  opencode: 'opencode',
  amp: 'Amp',
  devin: 'Devin CLI',
  agy: 'Antigravity CLI',
  cline: 'Cline CLI',
  kilo: 'Kilo Code',
  kimi: 'Kimi Code',
  kiro: 'Kiro CLI',
  droid: 'Droid',
  grok: 'Grok CLI',
  hermes: 'Hermes Agent',
  letta: 'Letta Code',
  maki: 'Maki',
  muse: 'Muse',
  qodercli: 'Qoder CLI',
  qwen: 'Qwen Code',
  pi: 'Pi',
  omp: 'Oh My Pi',
  mastracode: 'Mastra Code',
};

/** Short names for tight places (a push title, a sidebar row): the product name without "CLI"/"Code". */
const SHORT: Readonly<Record<string, string>> = {
  claude: 'Claude',
  codex: 'Codex',
  gemini: 'Gemini',
  cursor: 'Cursor',
  copilot: 'Copilot',
  devin: 'Devin',
  agy: 'Antigravity',
  cline: 'Cline',
  kilo: 'Kilo',
  kimi: 'Kimi',
  kiro: 'Kiro',
  grok: 'Grok',
  hermes: 'Hermes',
  letta: 'Letta',
  qodercli: 'Qoder',
  qwen: 'Qwen',
  mastracode: 'Mastra',
};

/** The agent's full name; an unknown kind as given (cut to 24 characters); no kind: "An agent". */
export function agentTitle(kind: unknown): string {
  if (typeof kind !== 'string' || !kind) return 'An agent';
  return AGENT_TITLES[kind] ?? kind.slice(0, 24);
}

/** The agent's short name (see SHORT), else its full name. */
export function agentShortName(kind: unknown): string {
  if (typeof kind === 'string' && SHORT[kind]) return SHORT[kind];
  return agentTitle(kind);
}

/** What an agent status means to a person, for screen readers and tooltips. */
export const AGENT_STATUS_TEXT: Readonly<Record<string, string>> = {
  working: 'working',
  blocked: 'needs you',
  done: 'finished',
  idle: 'idle',
  unknown: 'status unknown',
};

// ── helpers: an agent's in-process sub-agents (docs/protocol.md "Helpers") ──

/** The most children one `Agent.children` carries (the runner keeps no more; readers cut at it). */
export const MAX_AGENT_CHILDREN = 16;
/** Longest child id / name or model on the wire (readers cut longer ones). */
export const AGENT_CHILD_ID_MAX = 128;
export const AGENT_CHILD_NAME_MAX = 80;

export type AgentChildStatus = 'working' | 'done';
/** One sub-agent the pane's agent started inside its own process. */
export interface AgentChild {
  id: string;
  /** What the CLI calls it: its type or role name ("Explore", "tester"). */
  name: string;
  status: AgentChildStatus;
  /** When the status began (ISO 8601). */
  since: string;
  /** When it started; equal to `since` while working. */
  startedAt?: string;
  /** The child that started it, when the CLI says (a nested helper). */
  parentId?: string;
  model?: string;
  /** The role it was built from (`config.roles`), when the runner matched its name to one; absent otherwise. */
  roleId?: string;
  /** Open needs-you items it raised (not sent yet). */
  needs?: number;
}

/** Durable event types for helpers (`event.type`); `data` is `{kind, childId, name, parentId?, model?, roleId?}`. */
export const SUBAGENT_STARTED = 'subagent_started';
export const SUBAGENT_COMPLETED = 'subagent_completed';

const cut = (s: string, n: number) => (s.length > n ? s.slice(0, n) : s);

/**
 * `Agent.children` as a reader may trust it: never throws, drops entries that
 * are not a child, cuts long strings and the list at MAX_AGENT_CHILDREN.
 * Anything else (absent, an older runner) is no children.
 */
export function parseAgentChildren(v: unknown): AgentChild[] {
  if (!Array.isArray(v)) return [];
  const out: AgentChild[] = [];
  for (const raw of v) {
    if (out.length >= MAX_AGENT_CHILDREN) break;
    if (!raw || typeof raw !== 'object') continue;
    const o = raw as Record<string, unknown>;
    if (typeof o.id !== 'string' || !o.id || typeof o.since !== 'string') continue;
    if (o.status !== 'working' && o.status !== 'done') continue;
    const c: AgentChild = {
      id: cut(o.id, AGENT_CHILD_ID_MAX),
      name: typeof o.name === 'string' ? cut(o.name, AGENT_CHILD_NAME_MAX) : '',
      status: o.status,
      since: cut(o.since, 64),
    };
    if (typeof o.startedAt === 'string') c.startedAt = cut(o.startedAt, 64);
    if (typeof o.parentId === 'string' && o.parentId) c.parentId = cut(o.parentId, AGENT_CHILD_ID_MAX);
    if (typeof o.model === 'string' && o.model) c.model = cut(o.model, AGENT_CHILD_NAME_MAX);
    if (typeof o.roleId === 'string' && /^[a-z0-9-]{2,32}$/.test(o.roleId)) c.roleId = o.roleId;
    if (typeof o.needs === 'number' && Number.isInteger(o.needs) && o.needs > 0) c.needs = Math.min(o.needs, 99);
    out.push(c);
  }
  return out;
}

/** How many helpers are at work now (what the card and the pane header count). */
export function helpersWorking(children: unknown): number {
  return parseAgentChildren(children).filter((c) => c.status === 'working').length;
}

/** "1 helper", "2 helpers"; nothing for none (the label only shows with helpers at work). */
export function helpersText(n: number): string {
  return n > 0 ? `${n} ${n === 1 ? 'helper' : 'helpers'}` : '';
}
