/**
 * Headless tasks (docs/design/headless-tasks.md): the runner's `task.*`
 * methods. `runner/crates/runner/src/task.rs` is the machine's side and
 * `runner/api/runner-api.schema.json` the contract.
 *
 * A machine runs tasks only when its own owner turned automation on with at
 * least one label (`hello.runner.automation.labels`). The server's policy for
 * a service account travels inside each `task.start` (labels, roots, allowed
 * commands and agents) and the runner checks it again on the machine.
 */

/** Every task method. */
export const TASK_METHODS: readonly string[] = ['task.start', 'task.get', 'task.list', 'task.cancel'];
/** Methods that change a task: through web only, never a client socket. */
export const TASK_WEB_ONLY_METHODS: readonly string[] = ['task.start', 'task.cancel'];

export const TASK_ID_RE = /^[A-Za-z0-9_-]{1,64}$/;
export const TASK_MIN_TIMEOUT_MS = 1000;
export const TASK_MAX_TIMEOUT_MS = 24 * 60 * 60 * 1000;
/** A machine's automation label: 1–32 of a-z, 0-9 and `-`, starting with a letter or digit. */
export const AUTOMATION_LABEL_RE = /^[a-z0-9][a-z0-9-]{0,31}$/;
export const MAX_AUTOMATION_LABELS = 16;

/** Chat modes a task's agent may run in; never one that approves tools by itself. */
export const TASK_AGENT_MODES = ['ask_first', 'plan_first', 'auto_edit'] as const;
export type TaskAgentMode = (typeof TASK_AGENT_MODES)[number];

export type TaskKind = 'command' | 'agent';
export type TaskStatus = 'running' | 'done' | 'failed' | 'cancelled' | 'timed_out';
export type TaskReasonCode = 'exit_status' | 'spawn_failed' | 'timeout' | 'permission_needed' | 'taken_over' | 'closed' | 'lost';
export const TASK_FINAL_STATUSES: readonly TaskStatus[] = ['done', 'failed', 'cancelled', 'timed_out'];

/** `task.start` params (snake_case on the wire). */
export interface TaskStartParams {
  task_id: string;
  kind: TaskKind;
  /** The labels the policy allows; the machine must carry one of them. */
  labels: string[];
  cwd: string;
  roots: string[];
  argv?: string[];
  allowed_commands?: string[];
  agent?: string;
  allowed_agents?: string[];
  mode?: TaskAgentMode;
  text?: string;
  label?: string;
  timeout_ms?: number;
}

/** A task as the runner reports it (`task_info.task`). */
export interface TaskInfo {
  task_id: string;
  kind: TaskKind;
  label: string;
  cwd: string;
  status: TaskStatus;
  reason_code: TaskReasonCode | null;
  reason: string | null;
  exit_code: number | null;
  signal: string | null;
  output: string | null;
  output_truncated: boolean;
  pane_id: string | null;
  session_id: string | null;
  started_at: string;
  ended_at: string | null;
}

/** The machine's automation labels from its hello; [] when it never turned automation on. */
export function automationLabels(hello: unknown): string[] {
  const a = hello && typeof hello === 'object' ? (hello as { automation?: unknown }).automation : undefined;
  if (!a || typeof a !== 'object') return [];
  const labels = (a as { labels?: unknown }).labels;
  if (!Array.isArray(labels)) return [];
  const out: string[] = [];
  for (const l of labels) {
    if (typeof l === 'string' && AUTOMATION_LABEL_RE.test(l) && !out.includes(l)) out.push(l);
    if (out.length >= MAX_AUTOMATION_LABELS) break;
  }
  return out;
}
