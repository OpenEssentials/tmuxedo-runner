/**
 * "Needs you" through the plugin contract (docs/protocol.md "Needs you",
 * docs/design/multi-agent-workflows.md §2, PLAN Phase 13).
 *
 * The runner owns open items, one set per pane (`needs {paneId, items}`,
 * replace semantics like `topology`), and says when one opens or closes with
 * the durable events `needs_opened {item}` / `needs_resolved {id, by, …}`.
 * A person answers with `needs.respond` (a runner API method, sent by the
 * gateway's `machine.respond` and by a client's `req`), whose outcome is
 * `accepted | stale | unknown`, exactly as for a held prompt.
 *
 * One model for terminal and chat: a chat's items (`chat.ts NeedsItem`,
 * answered on the structured channel) become `NeedsYou` with
 * `answerVia: 'session'` through `fromChatItem`, and a held prompt from a
 * runner without the `needs` feature through `fromLegacyPrompt`, so web, the
 * app, push and the TUI draw one list.
 *
 * Rust twin: runner/crates/protocol/src/needs.rs; both check
 * fixtures/needs.json. Parsers never throw: junk is dropped.
 */

/** The hello feature of a runner that sends `needs` and answers `needs.respond`; also the welcome feature of a gateway that relays `needs`. */
export const NEEDS_FEATURE = 'needs';

/** `file`: an agent asked for a file (`tmuxedo files request`, Phase 15), answered by uploading one. */
export const NEEDS_KINDS = ['permission', 'question', 'plan', 'login', 'error', 'limit', 'review', 'file'] as const;
export type NeedsKind = (typeof NEEDS_KINDS)[number];

/**
 * What a person may answer with. `approve`/`deny` (+ optional text, sent as
 * the deny message), `reply` (free text), `choose` (one of `options`),
 * `answers` / `approve_plan` (chat), `action` (a manifest action, e.g. a
 * login), `open` (open the pane or chat: answered there), `dismiss`,
 * `upload` (a `file` item: upload a file to the item's project; the runner
 * closes the item when that upload lands).
 */
export const NEEDS_RESPONSE_KINDS = ['approve', 'deny', 'reply', 'choose', 'answers', 'approve_plan', 'action', 'open', 'dismiss', 'upload'] as const;
export type NeedsResponseKind = (typeof NEEDS_RESPONSE_KINDS)[number];

/** What answering needs: nothing, control of the machine, or a step-up (15 min; high risk: 5). */
export const REQUIRES = ['none', 'control', 'stepup'] as const;
export type Requires = (typeof REQUIRES)[number];

/**
 * How the runner delivers an answer (multi-agent-workflows.md §2.2, agent-chat.md §3.7).
 * `upload`: a file request; the answer is an upload carrying the item's
 * `detail.upload.requestId` (its `dismiss` goes through `needs.respond`).
 */
export const ANSWER_VIA = ['hook', 'keystroke', 'action', 'session', 'none', 'upload'] as const;
export type AnswerVia = (typeof ANSWER_VIA)[number];

/** Where an item came from: a verified agent hook, the screen, a `pane.report_agent` (never decidable), the core. */
export const NEEDS_SOURCES = ['hook', 'screen', 'report', 'core', 'chat'] as const;
export type NeedsSource = (typeof NEEDS_SOURCES)[number];

/** D-22 risk classes; any of these is high risk (only from the full sheet, a fresh step-up). */
export const RISK_CLASSES = ['git_push', 'install', 'admin', 'delete_outside', 'edit_outside', 'network', 'credentials'] as const;
export type RiskClass = (typeof RISK_CLASSES)[number];

export interface NeedsOption { id: string; label: string }

export interface NeedsResponse {
  /** Unique within the item: `approve`, `deny`, `reply`, `choose`, `login`, `open` … */
  id: string;
  kind: NeedsResponseKind;
  label: string;
  requires: Requires;
  /** `choose`: the choices (ids are what `optionId` names; for keystrokes, the key). */
  options?: NeedsOption[];
  /** `action`: the plugin's manifest action. */
  actionId?: string;
}

export interface NeedsYou {
  /** Runner-assigned, unique per machine (a held prompt's promptId is one). */
  id: string;
  /** "" for a machine-level item (a login). */
  paneId: string;
  /** A chat's item (answerVia `session`). */
  chatId?: string;
  plugin: string;
  kind: NeedsKind;
  /** One plain line, ≤ 120 characters, masked by the runner. */
  title: string;
  detail?: {
    tool?: string;
    input?: unknown;
    suggestions?: unknown[];
    /** A question's words (from the hook or the screen). */
    text?: string;
    message?: string;
    questions?: unknown[];
    markdown?: string;
    /** A `file` item: where the upload goes and the request it answers. */
    upload?: FileRequestDetail;
  };
  /** What came before it this turn ("read upload screenshot.png"). */
  context?: string[];
  risk: 'normal' | 'high';
  riskClass?: RiskClass;
  /** Why it is high risk, in plain words. */
  why?: string;
  responses: NeedsResponse[];
  answerVia: AnswerVia;
  /** `keystroke`: the screen fingerprint taken when it opened (sha-256 hex of the prompt region). */
  fingerprint?: string;
  source: NeedsSource;
  createdAt: string;
  expiresAt?: string;
}

export interface NeedsFrame { t: 'needs'; paneId: string; items: NeedsYou[] }

/** A `file` item's `detail.upload`: the project the file goes to and the request id an upload names. */
export interface FileRequestDetail { project: string; requestId: string }

/** The request id an upload may carry (`requestId` on POST …/uploads, `request_id` to the runner). */
export const FILE_REQUEST_ID = /^[A-Za-z0-9_-]{8,64}$/;

/** A `file` item's upload target, or null when the item is not a well-formed file request. */
export function fileRequestOf(item: Pick<NeedsYou, 'kind' | 'answerVia' | 'detail'>): FileRequestDetail | null {
  if (item.kind !== 'file' || item.answerVia !== 'upload') return null;
  const u = (item.detail as { upload?: unknown } | undefined)?.upload;
  if (!obj(u) || typeof u.requestId !== 'string' || !FILE_REQUEST_ID.test(u.requestId)) return null;
  if (typeof u.project !== 'string' || !u.project || u.project.length > 200) return null;
  return { project: u.project, requestId: u.requestId };
}

/** Who closed an item (`needs_resolved.by`). */
export const RESOLVED_BY = ['remote', 'terminal', 'expired', 'agent', 'screen_changed', 'cancelled'] as const;
export type ResolvedBy = (typeof RESOLVED_BY)[number];

export type RespondOutcome = 'accepted' | 'stale' | 'unknown';

/** `needs.respond` params (runner API; web's /api/needs/respond sends the same). */
export interface RespondParams {
  itemId: string;
  paneId?: string;
  chatId?: string;
  responseId: string;
  /** `reply`, or the message with a `deny`. */
  text?: string;
  /** `choose`. */
  optionId?: string;
  /** `answers` (chat questions). */
  answers?: Record<string, string | string[]>;
  /** Who answered, for the others' "Answered on iPhone" (≤ 120 characters). */
  by?: string;
  /** Set by web only: the caller stepped up in the last 5 minutes (high-risk approve). */
  fresh_stepup?: boolean;
}

/** Limits. */
export const NEEDS_MAX_ITEMS = 20;
export const NEEDS_MAX_TITLE = 120;
export const NEEDS_MAX_TEXT = 2000;
export const NEEDS_MAX_OPTIONS = 9;
export const NEEDS_ID = /^[A-Za-z0-9_.:-]{1,128}$/;
export const RESPONSE_ID = /^[a-z][a-z0-9_.-]{0,31}$/;
const OPTION_ID = /^[A-Za-z0-9_-]{1,32}$/;
const PLUGIN = /^[a-z0-9-]{2,32}$/;

const obj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === 'object' && !Array.isArray(v);
const str = (v: unknown, max: number): v is string => typeof v === 'string' && v.length <= max;

function parseResponse(v: unknown): NeedsResponse | null {
  if (!obj(v) || typeof v.id !== 'string' || !RESPONSE_ID.test(v.id)) return null;
  if (!(NEEDS_RESPONSE_KINDS as readonly string[]).includes(v.kind as string)) return null;
  if (typeof v.label !== 'string' || !v.label || v.label.length > 40) return null;
  if (!(REQUIRES as readonly string[]).includes(v.requires as string)) return null;
  const r: NeedsResponse = { id: v.id, kind: v.kind as NeedsResponseKind, label: v.label, requires: v.requires as Requires };
  if (v.options !== undefined) {
    if (!Array.isArray(v.options) || v.options.length > NEEDS_MAX_OPTIONS) return null;
    const opts: NeedsOption[] = [];
    for (const o of v.options) {
      if (!obj(o) || typeof o.id !== 'string' || !OPTION_ID.test(o.id) || typeof o.label !== 'string' || o.label.length > 200) return null;
      opts.push({ id: o.id, label: o.label });
    }
    r.options = opts;
  }
  if (r.kind === 'choose' && !r.options?.length) return null;
  if (v.actionId !== undefined) {
    if (typeof v.actionId !== 'string' || !/^[a-z0-9][a-z0-9._-]{0,63}$/.test(v.actionId)) return null;
    r.actionId = v.actionId;
  }
  if (r.kind === 'action' && !r.actionId) return null;
  return r;
}

/** One item, or null when it breaks a rule (a bad item is dropped, its frame kept). */
export function parseNeedsItem(v: unknown): NeedsYou | null {
  if (!obj(v)) return null;
  if (typeof v.id !== 'string' || !NEEDS_ID.test(v.id)) return null;
  if (typeof v.paneId !== 'string' || v.paneId.length > 255) return null;
  if (v.chatId !== undefined && (typeof v.chatId !== 'string' || !/^[A-Za-z0-9_-]{8,64}$/.test(v.chatId))) return null;
  if (typeof v.plugin !== 'string' || !PLUGIN.test(v.plugin)) return null;
  if (!(NEEDS_KINDS as readonly string[]).includes(v.kind as string)) return null;
  if (typeof v.title !== 'string' || !v.title || v.title.length > NEEDS_MAX_TITLE) return null;
  if (v.risk !== 'normal' && v.risk !== 'high') return null;
  if (v.riskClass !== undefined && !(RISK_CLASSES as readonly string[]).includes(v.riskClass as string)) return null;
  if (!(ANSWER_VIA as readonly string[]).includes(v.answerVia as string)) return null;
  if (!(NEEDS_SOURCES as readonly string[]).includes(v.source as string)) return null;
  if (!str(v.createdAt, 64) || (v.expiresAt !== undefined && !str(v.expiresAt, 64))) return null;
  if (v.fingerprint !== undefined && (typeof v.fingerprint !== 'string' || !/^[0-9a-f]{64}$/.test(v.fingerprint))) return null;
  if (v.answerVia === 'keystroke' && v.fingerprint === undefined) return null;
  if (v.answerVia === 'session' && v.chatId === undefined) return null;
  // A file request is answered by an upload, and only a file request is.
  if ((v.kind === 'file') !== (v.answerVia === 'upload')) return null;
  if (v.answerVia === 'upload' && !fileRequestOf({ kind: 'file', answerVia: 'upload', detail: obj(v.detail) ? v.detail : undefined })) return null;
  if (!Array.isArray(v.responses) || v.responses.length > 8) return null;
  const responses: NeedsResponse[] = [];
  for (const x of v.responses) {
    const r = parseResponse(x);
    if (!r || responses.some((o) => o.id === r.id)) return null;
    responses.push(r);
  }
  if (v.answerVia !== 'upload' && responses.some((r) => r.kind === 'upload')) return null;
  // A report (pane.report_agent) can never be decided remotely (not-yet-thought-of.md #1).
  if (v.source === 'report' && responses.some((r) => r.kind !== 'open' && r.kind !== 'dismiss')) return null;
  const item: NeedsYou = {
    id: v.id, paneId: v.paneId, plugin: v.plugin, kind: v.kind as NeedsKind, title: v.title,
    risk: v.risk, responses, answerVia: v.answerVia as AnswerVia, source: v.source as NeedsSource, createdAt: v.createdAt,
  };
  if (v.chatId !== undefined) item.chatId = v.chatId as string;
  if (obj(v.detail)) item.detail = v.detail as NeedsYou['detail'];
  if (Array.isArray(v.context)) item.context = v.context.filter((c): c is string => typeof c === 'string').slice(0, 8).map((c) => c.slice(0, 200));
  if (v.riskClass !== undefined) item.riskClass = v.riskClass as RiskClass;
  if (typeof v.why === 'string') item.why = v.why.slice(0, 200);
  if (v.fingerprint !== undefined) item.fingerprint = v.fingerprint as string;
  if (v.expiresAt !== undefined) item.expiresAt = v.expiresAt as string;
  return item;
}

/** A runner `needs` frame (items checked one by one); null when the frame itself is junk. */
export function parseNeedsFrame(o: Record<string, unknown>): NeedsFrame | null {
  if (typeof o.paneId !== 'string' || o.paneId.length > 255) return null;
  if (!Array.isArray(o.items) || o.items.length > NEEDS_MAX_ITEMS) return null;
  const items: NeedsYou[] = [];
  for (const x of o.items) {
    const it = parseNeedsItem(x);
    // An item belongs to the pane whose set it is in; the same id twice is dropped.
    if (it && it.paneId === o.paneId && !items.some((i) => i.id === it.id)) items.push(it);
  }
  return { t: 'needs', paneId: o.paneId, items };
}

/** Validate `needs.respond` params (web, gateway and runner refuse the same things). */
export function parseRespondParams(v: unknown): { ok: true; params: RespondParams } | { ok: false; reason: string } {
  if (!obj(v)) return { ok: false, reason: 'not_an_object' };
  if (typeof v.itemId !== 'string' || !NEEDS_ID.test(v.itemId)) return { ok: false, reason: 'bad_item_id' };
  if (typeof v.responseId !== 'string' || !RESPONSE_ID.test(v.responseId)) return { ok: false, reason: 'bad_response_id' };
  if (v.paneId !== undefined && (typeof v.paneId !== 'string' || v.paneId.length > 255)) return { ok: false, reason: 'bad_pane_id' };
  if (v.chatId !== undefined && (typeof v.chatId !== 'string' || !/^[A-Za-z0-9_-]{8,64}$/.test(v.chatId))) return { ok: false, reason: 'bad_chat_id' };
  if (v.text !== undefined && (typeof v.text !== 'string' || v.text.length > NEEDS_MAX_TEXT || /[\u0000-\u0008\u000b-\u001f\u007f\u009b]/.test(v.text))) return { ok: false, reason: 'bad_text' };
  if (v.optionId !== undefined && (typeof v.optionId !== 'string' || !OPTION_ID.test(v.optionId))) return { ok: false, reason: 'bad_option' };
  if (v.by !== undefined && (typeof v.by !== 'string' || v.by.length > 120)) return { ok: false, reason: 'bad_by' };
  if (v.fresh_stepup !== undefined && typeof v.fresh_stepup !== 'boolean') return { ok: false, reason: 'bad_fresh_stepup' };
  if (v.answers !== undefined && !obj(v.answers)) return { ok: false, reason: 'bad_answers' };
  const p: RespondParams = { itemId: v.itemId, responseId: v.responseId };
  if (v.paneId !== undefined) p.paneId = v.paneId as string;
  if (v.chatId !== undefined) p.chatId = v.chatId as string;
  if (v.text !== undefined) p.text = v.text as string;
  if (v.optionId !== undefined) p.optionId = v.optionId as string;
  if (v.answers !== undefined) p.answers = v.answers as RespondParams['answers'];
  if (v.by !== undefined) p.by = v.by as string;
  if (v.fresh_stepup !== undefined) p.fresh_stepup = v.fresh_stepup as boolean;
  return { ok: true, params: p };
}

/** The response an item offers under `id`, or undefined. */
export function findResponse(item: Pick<NeedsYou, 'responses'>, id: string): NeedsResponse | undefined {
  return item.responses.find((r) => r.id === id);
}

/** Whether answering with this response needs a FRESH step-up (≤ 5 minutes): approving a high-risk item. */
export function needsFreshStepUp(item: Pick<NeedsYou, 'risk'>, r: Pick<NeedsResponse, 'kind'>): boolean {
  return item.risk === 'high' && (r.kind === 'approve' || r.kind === 'answers' || r.kind === 'approve_plan' || r.kind === 'choose' || r.kind === 'reply');
}

// ── push categories (fixed: iOS registers them at build time) ────────────

export const NEEDS_CATEGORIES = ['NEEDS_APPROVE_DENY', 'NEEDS_REPLY', 'NEEDS_OPEN'] as const;
export type NeedsCategory = (typeof NEEDS_CATEGORIES)[number];
/** Notification action ids, the same on web push and native. */
export const NOTIFICATION_ACTIONS = { approve: 'approve', deny: 'deny', reply: 'reply', open: 'open' } as const;

/**
 * The richest category an item's responses fit. High risk never gets one with
 * an approve (D-22: only from the full sheet); anything else opens the app.
 */
export function needsCategory(item: Pick<NeedsYou, 'risk' | 'responses' | 'answerVia'>): NeedsCategory {
  const kinds = new Set(item.responses.map((r) => r.kind));
  if (item.risk === 'normal' && kinds.has('approve') && kinds.has('deny') && item.answerVia !== 'none') return 'NEEDS_APPROVE_DENY';
  if (item.risk === 'normal' && kinds.has('reply') && item.answerVia !== 'none') return 'NEEDS_REPLY';
  return 'NEEDS_OPEN';
}

/**
 * What a notification action answers, or null when the action must open the
 * full sheet instead: approving a high-risk item, an action the category does
 * not have, or a reply with no text.
 */
export function notificationResponse(
  item: Pick<NeedsYou, 'risk' | 'responses' | 'answerVia'>,
  action: string,
  text?: string,
): { responseId: string; text?: string } | null {
  const cat = needsCategory(item);
  if (cat === 'NEEDS_APPROVE_DENY' && (action === 'approve' || action === 'deny')) {
    const r = item.responses.find((x) => x.kind === action);
    return r ? { responseId: r.id } : null;
  }
  if (cat === 'NEEDS_REPLY' && action === 'reply') {
    const t = (text ?? '').trim();
    const r = item.responses.find((x) => x.kind === 'reply');
    return r && t ? { responseId: r.id, text: t.slice(0, NEEDS_MAX_TEXT) } : null;
  }
  return null;
}

// ── one list: chats and older runners ────────────────────────────────────

const STEPUP: Requires = 'stepup';

/** The responses of a permission item answered through a held hook. */
export function permissionResponses(): NeedsResponse[] {
  return [
    { id: 'approve', kind: 'approve', label: 'Approve', requires: STEPUP },
    { id: 'deny', kind: 'deny', label: 'Deny', requires: STEPUP },
  ];
}

/**
 * A held prompt from a runner WITHOUT the `needs` feature (Agent.promptId /
 * Agent.prompt), as an item: id = promptId, answered through the hook.
 */
export function fromLegacyPrompt(p: {
  paneId: string; promptId: string; kind: string | null; tool: string; input: unknown; suggestions?: unknown[]; title: string; since: string | null;
}): NeedsYou {
  return {
    id: p.promptId, paneId: p.paneId, plugin: p.kind && PLUGIN.test(p.kind) ? p.kind : 'claude', kind: 'permission',
    title: p.title.slice(0, NEEDS_MAX_TITLE) || 'Permission needed',
    detail: { tool: p.tool, input: p.input, ...(p.suggestions?.length ? { suggestions: p.suggestions } : {}) },
    risk: 'normal', responses: permissionResponses(), answerVia: 'hook', source: 'hook', createdAt: p.since ?? new Date(0).toISOString(),
  };
}

/** A chat's item (chat.ts NeedsItem, `chat_needs`) as a NeedsYou (answerVia `session`). */
export function fromChatItem(
  sessionId: string,
  it: { id: string; kind: string; risk: string; title: string; options?: readonly string[]; tool?: string; input?: unknown; why?: string; questions?: unknown[]; markdown?: string; expiresAt?: string },
  plugin = 'claude',
  createdAt = new Date().toISOString(),
): NeedsYou {
  const kind: NeedsKind = it.kind === 'question' ? 'question' : it.kind === 'plan' ? 'plan' : 'permission';
  const opts = new Set(it.options ?? []);
  const responses: NeedsResponse[] = [];
  if (opts.has('allow_once')) responses.push({ id: 'allow_once', kind: 'approve', label: 'Allow once', requires: STEPUP });
  if (opts.has('allow_session')) responses.push({ id: 'allow_session', kind: 'approve', label: 'Allow for this chat', requires: STEPUP });
  if (opts.has('answers')) responses.push({ id: 'answers', kind: 'answers', label: 'Answer', requires: STEPUP });
  if (opts.has('approve_plan')) responses.push({ id: 'approve_plan', kind: 'approve_plan', label: 'Approve plan', requires: STEPUP });
  if (opts.has('keep_planning')) responses.push({ id: 'keep_planning', kind: 'deny', label: 'Keep planning', requires: STEPUP });
  if (opts.has('deny')) responses.push({ id: 'deny', kind: 'deny', label: 'Deny', requires: STEPUP });
  responses.push({ id: 'open', kind: 'open', label: 'Open chat', requires: 'none' });
  return {
    id: it.id, paneId: '', chatId: sessionId, plugin: PLUGIN.test(plugin) ? plugin : 'claude', kind,
    title: (it.title || 'Needs you').slice(0, NEEDS_MAX_TITLE),
    detail: { ...(it.tool ? { tool: it.tool } : {}), ...(it.input !== undefined ? { input: it.input } : {}), ...(it.questions ? { questions: it.questions } : {}), ...(it.markdown ? { markdown: it.markdown } : {}) },
    risk: it.risk === 'normal' ? 'normal' : 'high', ...(it.why ? { why: it.why } : {}),
    responses: responses.slice(0, 8), answerVia: 'session', source: 'chat', createdAt, ...(it.expiresAt ? { expiresAt: it.expiresAt } : {}),
  };
}

// ── links: verified or not (not-yet-thought-of.md #1) ────────────────────

/**
 * Whether `host` is on an allowlist. Entries are a host (`claude.ai`, exact)
 * or `*.example.com` (any subdomain, not the bare domain). Case-insensitive;
 * a trailing dot is ignored; IP literals and userinfo never match a pattern.
 */
export function hostAllowed(host: string, allow: readonly string[]): boolean {
  const h = host.toLowerCase().replace(/\.$/, '');
  if (!h || !/^[a-z0-9.-]+$/.test(h)) return false;
  return allow.some((raw) => {
    const a = raw.toLowerCase().replace(/\.$/, '');
    if (a.startsWith('*.')) {
      const base = a.slice(2);
      return !!base && h.endsWith(`.${base}`) && h.length > base.length + 1;
    }
    return a === h;
  });
}

/** The host a link points at, for display ("" when unparseable). Userinfo is never shown as the host. */
export function linkHost(url: string): string {
  try {
    const u = new URL(url);
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return '';
    return u.hostname.toLowerCase();
  } catch {
    return '';
  }
}

/**
 * How a client should show an `open_url` event's link: `verified` (from the
 * pane's agent, host on its plugin's list), `unverified` (the runner said so:
 * another process in the pane, or a host not on the list) or `legacy` (an
 * older runner that does not check: shown as before).
 */
export function openUrlTrust(data: unknown): { trust: 'verified' | 'unverified' | 'legacy'; host: string; reason?: string } {
  const d = obj(data) ? data : {};
  const host = typeof d.url === 'string' ? linkHost(d.url) : '';
  if (d.verified === true) return { trust: 'verified', host };
  if (d.verified === false) return { trust: 'unverified', host, ...(typeof d.reason === 'string' ? { reason: d.reason.slice(0, 64) } : {}) };
  return { trust: 'legacy', host };
}

/** The words for an unverified link (web, app and TUI say the same). */
export function unverifiedLinkText(host: string, where: string): string {
  return `Unverified link to ${host || 'an unknown site'} from a process in ${where}. Open it only if you started it.`;
}
