/**
 * Agent chat (docs/protocol.md "Agent chat", docs/design/agent-chat.md §3):
 * the normalised, ACP-shaped event model a runner produces from an agent CLI
 * in its structured mode, the CHAT frame that carries one event, and the
 * runner API shapes (`chat.*`) web, the app and the Phase 18 headless API use.
 *
 * Rust twin: runner/crates/protocol/src/chat.rs. Both are held to
 * fixtures/chat.json (frames byte for byte, every event type, junk refused).
 *
 * The gateway never reads a CHAT payload: it routes on the header (session
 * id, seq, flags) exactly as it does pane bytes. Clients parse the payload
 * with `decodeChatFrame` / `parseChatEvent`, which never throw.
 */
import { CHAT_SESSION_ID, DataKind, decodeData, encodeData, FLAG_EPHEMERAL } from './frames.js';

/** An event is at most this large after masking (bigger fields are cut, `truncated: true`). */
export const CHAT_MAX_EVENT_BYTES = 256 * 1024;
/** Tool output kept in an event: its head for reads, searches and fetches, its tail otherwise (`outputKept`). */
export const CHAT_MAX_TOOL_OUTPUT = 16 * 1024;
/** A tool's `name` and `server` on `tool.started`, in characters. */
export const CHAT_MAX_TOOL_NAME = 64;
/** A tool's `subject` on `tool.started`, in characters (one line). */
export const CHAT_MAX_TOOL_SUBJECT = 300;
/** Attachments listed on one `tool.done` (their type and size, never their bytes). */
export const CHAT_MAX_TOOL_ATTACHMENTS = 20;
/** A message sent to an agent (`chat.send` text), in UTF-8 bytes. */
export const CHAT_MAX_SEND_BYTES = 64 * 1024;
/** Days a chat's events stay on the machine (D-C6). */
export const CHAT_HISTORY_DAYS = 30;

export const CHAT_EVENT_TYPES = [
  'session.opened', 'message.user', 'message.delta', 'message.done', 'thought.done',
  'tool.started', 'tool.updated', 'tool.done', 'plan.updated', 'needs.opened', 'needs.resolved',
  'files.changed', 'turn.started', 'turn.done', 'usage', 'session.mode', 'session.handoff',
  'session.closed', 'notice',
] as const;
export type ChatEventType = (typeof CHAT_EVENT_TYPES)[number];

/**
 * `auto_approve` is the opt-in "Auto-approve" (the CLI's bypass mode): it
 * needs a fresh step-up (web sets `fresh_stepup`), an admin may forbid it
 * (`config.chat.autoApprove`), and the runner's guard still blocks the
 * high-risk "never" list.
 */
export const CHAT_MODES = ['ask_first', 'plan_first', 'auto_edit', 'auto_approve'] as const;
export type ChatMode = (typeof CHAT_MODES)[number];
export const AUTO_APPROVE: ChatMode = 'auto_approve';

/** `chat.file` (the Files panel's Preview · Raw · Changes): one file of the chat's work folder. */
export interface ChatFile {
  type: 'chat_file';
  /** Relative to the chat's work folder. */
  path: string;
  absPath?: string;
  mime: string;
  size: number;
  mtime?: number;
  /** Masked text (≤ 2 MiB; `truncated` when there is more). */
  text?: string | null;
  /** A small image, base64. */
  data?: string | null;
  truncated: boolean;
  binary: boolean;
  changes:
    | { kind: 'new' }
    | { kind: 'tool'; diffs: Array<{ oldText: string; newText: string }> }
    | { kind: 'git'; patch: string }
    | { kind: 'none' };
}
export const LOGIN_KINDS = ['api_key', 'cloud', 'subscription'] as const;
export type LoginKind = (typeof LOGIN_KINDS)[number];
export const CHAT_STATUSES = ['idle', 'working', 'needs_you', 'parked', 'terminal', 'closed'] as const;
export type ChatStatus = (typeof CHAT_STATUSES)[number];
/** What a person may answer a needs-you item with (its `options` say which apply). */
export const RESPONSE_KINDS = ['allow_once', 'allow_session', 'deny', 'answers', 'approve_plan', 'keep_planning'] as const;
export type ResponseKind = (typeof RESPONSE_KINDS)[number];
/** ACP tool kinds. */
export type ToolKind = 'read' | 'edit' | 'delete' | 'move' | 'search' | 'execute' | 'think' | 'fetch' | 'other';
/**
 * A refinement of `kind` (`kind` keeps its nine values). A runner may send a
 * value newer than this list: show the kind's default for one you do not know.
 */
export const TOOL_VARIANTS = ['list', 'create', 'glob', 'web_search', 'mcp', 'agent', 'plan', 'question'] as const;
export type ToolVariant = (typeof TOOL_VARIANTS)[number];
/**
 * How a tool call ended. `denied`: a person or a rule said no and the call
 * did not run. `cancelled`: it was still open when its turn was interrupted.
 * Runners before these existed sent `failed` for a denied call and nothing
 * for an interrupted one.
 */
export const TOOL_STATUSES = ['completed', 'failed', 'denied', 'cancelled'] as const;
export type ToolStatus = (typeof TOOL_STATUSES)[number];
/** Something other than text in a tool's result: what it is and how big, never the bytes. */
export interface ToolAttachment { mime: string; bytes: number }

export interface QuestionOption { label: string; description?: string; preview?: string }
export interface Question { text: string; header?: string; options: QuestionOption[]; multi: boolean }

/**
 * A needs-you item raised by a chat (`answerVia: session`): exact and bound to
 * its id. `risk: 'high'` items offer Allow once / Deny only and need a fresh
 * step-up (≤ 5 minutes) to allow.
 */
export interface NeedsItem {
  id: string;
  kind: 'permission' | 'question' | 'plan';
  risk: 'normal' | 'high';
  options: ResponseKind[];
  /** One plain line: "Run `npm test`", "Answer 2 questions", "Approve the plan". */
  title: string;
  tool?: string;
  input?: unknown;
  /** Why it is high risk, in plain words. */
  why?: string;
  questions?: Question[];
  markdown?: string;
  expiresAt: string;
}

interface Envelope<T extends ChatEventType> {
  /** 1, 2, 3 … per session; 0 on an ephemeral `message.delta`. */
  seq: number;
  at: string;
  sessionId: string;
  turnId?: string;
  /** A sub-agent's path (its parent tool id). */
  agentPath?: string;
  type: T;
  /** Set when a field was cut to fit CHAT_MAX_EVENT_BYTES. */
  truncated?: boolean;
}

export type ChatEvent =
  | Envelope<'session.opened'> & { plugin: string; cwd: string; mode: string; login: LoginKind | ''; model?: string; features: string[]; resumedFrom?: string }
  | Envelope<'message.user'> & { messageId: string; text: string; attachments?: unknown[]; by?: string; clientMsgId?: string; queued?: boolean }
  | Envelope<'message.delta'> & { messageId: string; text: string }
  | Envelope<'message.done'> & { messageId: string; text: string }
  | Envelope<'thought.done'> & {
    /** Empty when `redacted`. */
    text: string;
    /** How long the agent thought, where its stream says. */
    durationMs?: number;
    /** The vendor hid this thought: there is no text to show. */
    redacted?: boolean;
  }
  | Envelope<'tool.started'> & {
    toolId: string; kind: ToolKind;
    /** A whole line in the past tense ("Ran `npm test`"): what a reader shows when there is no `subject`. */
    title: string;
    input: unknown; locations?: Array<{ path: string; line?: number }>;
    /** The tool's own name ("Bash", "create_issue"). */
    name?: string;
    /** The MCP server the tool belongs to. */
    server?: string;
    /** The object of the call (the command, the path, the pattern, the address): one line, masked. The reader writes the verb. */
    subject?: string;
    variant?: ToolVariant;
  }
  | Envelope<'tool.updated'> & {
    toolId: string; status: string;
    /** The latest tail of the call's output so far: it replaces the previous one, it is not appended. */
    output?: string;
    /** `line`: the first line of the change in the file (1-based), where the agent says. */
    diff?: { path: string; oldText: string; newText: string; line?: number };
  }
  | Envelope<'tool.done'> & {
    toolId: string; status: ToolStatus; summary?: string; output?: string;
    exitCode?: number;
    /** The agent's own time for the call (without it: the difference of the two events' `at`). */
    durationMs?: number;
    /** The size of the whole output, of which `output` is a part. */
    outputBytes?: number;
    /** Set when `output` is a cut of a longer one: which end was kept. */
    outputKept?: 'head' | 'tail';
    attachments?: ToolAttachment[];
    /** Matches or files found, where the agent gives a count. */
    resultCount?: number;
  }
  | Envelope<'plan.updated'> & { entries: Array<{ text: string; status: 'pending' | 'in_progress' | 'done' }> }
  | Envelope<'needs.opened'> & { item: NeedsItem }
  | Envelope<'needs.resolved'> & { id: string; outcome: 'answered' | 'expired' | 'cancelled'; by?: string; response: unknown }
  | Envelope<'files.changed'> & { files: Array<{ path: string; change: 'created' | 'modified' | 'deleted'; source: 'tool' | 'watch'; toolId?: string }> }
  | Envelope<'turn.started'>
  | Envelope<'turn.done'> & { status: 'completed' | 'interrupted' | 'failed'; error?: string; usage?: Record<string, number> }
  | Envelope<'usage'> & { usage: unknown }
  | Envelope<'session.mode'> & { mode: string; model?: string }
  | Envelope<'session.handoff'> & { to: 'terminal' | 'chat'; paneId?: string }
  | Envelope<'session.closed'> & { reason: string }
  | Envelope<'notice'> & { code: string; text: string; native?: string };

/** `chat.list` / `chat.get` session summary (the runner's projection; no conversation content beyond a masked preview). */
export interface ChatSession {
  id: string;
  plugin: string;
  cwd: string;
  mode: string;
  login: LoginKind | '';
  title: string;
  status: ChatStatus;
  createdAt: string;
  lastAt: string;
  preview: string;
  /** Who wrote the message `preview` is cut from. Absent from an older runner, and for a record written before it was kept: draw the preview as the agent's. */
  previewBy?: 'agent' | 'you';
  firstSeq: number;
  lastSeq: number;
  needs: number;
  queued: number;
  paneId: string | null;
  vendorSessionId: string;
  model: string | null;
}

/** `chat.respond` `response`. `answers`: question text (or index "0".."3") → label, labels, or free text ("Other…"). */
export interface ChatResponse { kind: ResponseKind; message?: string; answers?: Record<string, string | string[]> }

const str = (v: unknown, max = 1 << 20): v is string => typeof v === 'string' && v.length <= max;
const optStr = (v: unknown) => v === undefined || str(v);
const isSeq = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
const obj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === 'object' && !Array.isArray(v);

/** Per type: the fields an event must have, by JS type. */
const REQUIRED: Record<ChatEventType, Record<string, 'string' | 'object' | 'array' | 'any'>> = {
  'session.opened': { plugin: 'string', cwd: 'string', mode: 'string', login: 'string', features: 'array' },
  'message.user': { messageId: 'string', text: 'string' },
  'message.delta': { messageId: 'string', text: 'string' },
  'message.done': { messageId: 'string', text: 'string' },
  'thought.done': { text: 'string' },
  'tool.started': { toolId: 'string', kind: 'string', title: 'string', input: 'any' },
  'tool.updated': { toolId: 'string', status: 'string' },
  'tool.done': { toolId: 'string', status: 'string' },
  'plan.updated': { entries: 'array' },
  'needs.opened': { item: 'object' },
  'needs.resolved': { id: 'string', outcome: 'string', response: 'any' },
  'files.changed': { files: 'array' },
  'turn.started': {},
  'turn.done': { status: 'string' },
  usage: { usage: 'any' },
  'session.mode': { mode: 'string' },
  'session.handoff': { to: 'string' },
  'session.closed': { reason: 'string' },
  notice: { code: 'string', text: 'string' },
};

const optCount = (v: unknown) => v === undefined || isSeq(v);
const optInt = (v: unknown) => v === undefined || (typeof v === 'number' && Number.isSafeInteger(v));
const optBool = (v: unknown) => v === undefined || typeof v === 'boolean';

/**
 * The optional fields of the tool and thought events: absent is fine (an
 * older runner), a value of the wrong type is not an event (the Rust twin
 * refuses it too). Strings are not held to a list: `variant`, `status` and
 * `outputKept` may gain values.
 */
function optionalOk(type: ChatEventType, v: Record<string, unknown>): boolean {
  switch (type) {
    case 'thought.done':
      return optCount(v.durationMs) && optBool(v.redacted);
    case 'tool.started':
      return optStr(v.name) && optStr(v.server) && optStr(v.subject) && optStr(v.variant);
    case 'tool.updated':
      return optStr(v.output) && (v.diff === undefined || (obj(v.diff) && str(v.diff.path) && optCount(v.diff.line)));
    case 'tool.done':
      return optStr(v.summary) && optStr(v.output) && optInt(v.exitCode) && optCount(v.durationMs) && optCount(v.outputBytes)
        && optStr(v.outputKept) && optCount(v.resultCount)
        && (v.attachments === undefined || (Array.isArray(v.attachments) && v.attachments.every((a) => obj(a) && str(a.mime, 256) && isSeq(a.bytes))));
    default:
      return true;
  }
}

function isItem(v: unknown): v is NeedsItem {
  if (!obj(v)) return false;
  return str(v.id, 128) && ['permission', 'question', 'plan'].includes(v.kind as string) && ['normal', 'high'].includes(v.risk as string)
    && Array.isArray(v.options) && v.options.every((o) => (RESPONSE_KINDS as readonly string[]).includes(o as string))
    && str(v.title) && str(v.expiresAt, 64) && (v.questions === undefined || Array.isArray(v.questions)) && optStr(v.markdown);
}

/** A parsed event, or null for anything that is not one (never throws). */
export function parseChatEvent(input: string | Uint8Array): ChatEvent | null {
  let text: string;
  try {
    text = typeof input === 'string' ? input : new TextDecoder('utf-8', { fatal: true }).decode(input);
  } catch {
    return null;
  }
  if (text.length > CHAT_MAX_EVENT_BYTES + 4096) return null;
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    return null;
  }
  if (!obj(v)) return null;
  if (!isSeq(v.seq) || !str(v.at, 64) || typeof v.sessionId !== 'string' || !CHAT_SESSION_ID.test(v.sessionId)) return null;
  if (!optStr(v.turnId) || !optStr(v.agentPath)) return null;
  const type = v.type as ChatEventType;
  const need = Object.prototype.hasOwnProperty.call(REQUIRED, type) ? REQUIRED[type] : undefined;
  if (!need) return null;
  for (const [k, t] of Object.entries(need)) {
    const x = v[k];
    if (t === 'any' ? !(k in v) : t === 'array' ? !Array.isArray(x) : t === 'object' ? !obj(x) : typeof x !== 'string') return null;
  }
  if (type === 'needs.opened' && !isItem(v.item)) return null;
  if (!optionalOk(type, v)) return null;
  // Only a delta is ephemeral (seq 0); every other event has a seq.
  if ((type === 'message.delta') !== (v.seq === 0)) return null;
  return v as unknown as ChatEvent;
}

export interface ChatFrame {
  sessionId: string;
  /** Durable: the event's seq. Ephemeral: the last durable seq before it. */
  seq: number;
  ephemeral: boolean;
  /** Events before this one were pruned (30 days) or lost. */
  gap: boolean;
  /** 0 = live; else the replay this answers (a gateway strips it for clients: always 0 there). */
  req: number;
  /** The event, or null when the payload is not one this version understands. */
  event: ChatEvent | null;
}

/** Decode a binary CHAT frame (null for anything else). */
export function decodeChatFrame(buf: Uint8Array): ChatFrame | null {
  const f = decodeData(buf);
  if (!f || f.kind !== DataKind.CHAT || !CHAT_SESSION_ID.test(f.paneId) || f.offset > BigInt(Number.MAX_SAFE_INTEGER)) return null;
  return { sessionId: f.paneId, seq: Number(f.offset), ephemeral: !!f.ephemeral, gap: f.gap, req: f.req, event: parseChatEvent(f.payload) };
}

/** Encode a CHAT frame (runners and tests; the gateway never builds one). */
export function encodeChatFrame(f: { sessionId: string; seq: number; ephemeral?: boolean; gap?: boolean; req?: number; event: unknown }): Uint8Array {
  if (!CHAT_SESSION_ID.test(f.sessionId)) throw new RangeError('bad chat session id');
  return encodeData({
    kind: DataKind.CHAT, gap: !!f.gap, ephemeral: !!f.ephemeral, req: f.req ?? 0, paneId: f.sessionId,
    offset: BigInt(f.seq), payload: new TextEncoder().encode(typeof f.event === 'string' ? f.event : JSON.stringify(f.event)),
  });
}

/** Whether a flags byte is valid for a CHAT frame (for tests). */
export const CHAT_FLAGS = FLAG_EPHEMERAL | 0x01;
