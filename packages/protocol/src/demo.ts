/**
 * The simulated demo machine (App Store review accounts, queue 3d): its fixed
 * identity and topology, shared by the gateway's simulator
 * (gateway/src/demo.ts), which plays the runner side, and web, which seeds the
 * projection when it creates the machine so the first screen is not empty.
 *
 * Nothing here runs anything: it is data. PURE, no I/O.
 */
import type { Agent, Pane, RunnerInfo, Tab, Workspace } from './frames.js';

/** The name every review account's demo machine gets (the App Store review notes name it). */
export const DEMO_MACHINE_NAME = 'tmuxedo-demo';
/** `hello.runner.version` of the simulator; web shows it like any runner version. */
export const DEMO_RUNNER_VERSION = 'demo';
export const DEMO_HOSTNAME = 'tmuxedo-demo';
export const DEMO_CWD = '/home/reviewer/acme-api';

/** Pane ids in the runner's `w<n>:p<n>` form. */
export const DEMO_PANES = { shell: 'w1:p1', agent: 'w1:p2', notes: 'w1:p3' } as const;
export const DEMO_COLS = 100;
export const DEMO_ROWS = 30;

export function demoHello(since: string): RunnerInfo {
  return {
    version: DEMO_RUNNER_VERSION,
    holderVersion: DEMO_RUNNER_VERSION,
    os: 'linux',
    arch: 'x86_64',
    hostname: DEMO_HOSTNAME,
    claudeVersion: '2.1.0 (demo)',
    claudeAuth: { state: 'ok', reason: 'demo machine', since },
    features: ['glance'],
  };
}

export interface DemoTopology {
  t: 'topology';
  rev: number;
  workspaces: Workspace[];
  tabs: Tab[];
  panes: Pane[];
  focusedWorkspace: string;
}

/**
 * One workspace, two tabs: `main` is a horizontal split (shell | agent),
 * `notes` one more shell. `agent` is the agent pane's current state.
 */
export function demoTopology(rev: number, agent: Agent | null, size: { cols: number; rows: number } = { cols: DEMO_COLS, rows: DEMO_ROWS }): DemoTopology {
  const pane = (id: string, tabId: string, command: string, title: string, a: Agent | null): Pane => ({
    id, tabId, cwd: DEMO_CWD, command, title, cols: size.cols, rows: size.rows, agent: a,
  });
  return {
    t: 'topology',
    rev,
    focusedWorkspace: 'w1',
    workspaces: [{ id: 'w1', name: 'acme-api', cwd: DEMO_CWD, position: 0, activeTab: 'w1:t1' }],
    tabs: [
      { id: 'w1:t1', workspaceId: 'w1', name: 'main', position: 0, focusedPane: DEMO_PANES.agent, layout: { dir: 'h', ratio: 0.5, a: { pane: DEMO_PANES.shell }, b: { pane: DEMO_PANES.agent } } },
      { id: 'w1:t2', workspaceId: 'w1', name: 'notes', position: 1, focusedPane: DEMO_PANES.notes, layout: { pane: DEMO_PANES.notes } },
    ],
    panes: [
      pane(DEMO_PANES.shell, 'w1:t1', 'bash', 'shell', null),
      pane(DEMO_PANES.agent, 'w1:t1', 'claude', 'claude', agent),
      pane(DEMO_PANES.notes, 'w1:t2', 'bash', 'notes', null),
    ],
  };
}
