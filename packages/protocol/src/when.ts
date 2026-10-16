/**
 * Plugin visibility conditions (`when`, docs/design/plugins.md §6.1): a tiny
 * expression language over a fixed context (`auth.state`, `agent.status`,
 * `agent.kind`, `machine.kind`, `machine.online`, `settings.*` …). No calls, no
 * loops. The runner (runner/crates/protocol/src/when.rs) and every client
 * evaluate it identically; fixtures/when.json is checked by both, so the two
 * implementations cannot drift.
 *
 *   expr  := or
 *   or    := and (("or" | "||") and)*
 *   and   := not (("and" | "&&") not)*
 *   not   := ("not" | "!") not | cmp
 *   cmp   := atom (("==" | "!=") atom | "not"? "in" list)?
 *   atom  := string | number | true | false | null | path | "(" expr ")"
 *   list  := "[" (literal ("," literal)*)? "]"
 *   path  := ident ("." ident)*        ident = [a-z_][a-z0-9_]*
 *
 * A path that is missing from the context is null. A bare atom is true unless
 * it is null, false, "" or 0. Comparisons are strict (no coercion). Parsing
 * fails on junk (the manifest linter reports it); evaluation never throws.
 */

export type WhenValue = string | number | boolean | null;
export type WhenNode =
  | { k: 'lit'; v: WhenValue }
  | { k: 'path'; p: string[] }
  | { k: 'not'; a: WhenNode }
  | { k: 'and' | 'or'; a: WhenNode; b: WhenNode }
  | { k: 'eq' | 'ne'; a: WhenNode; b: WhenNode }
  | { k: 'in'; a: WhenNode; list: WhenValue[]; neg: boolean };

/** Longest expression a manifest may carry, and the deepest nesting. */
export const WHEN_MAX_LEN = 512;
export const WHEN_MAX_DEPTH = 16;

type Tok = { t: 'str'; v: string } | { t: 'num'; v: number } | { t: 'id'; v: string } | { t: 'op'; v: string };

function lex(src: string): Tok[] {
  const out: Tok[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i]!;
    if (c === ' ' || c === '\t' || c === '\n') { i++; continue; }
    if (c === "'" || c === '"') {
      let j = i + 1;
      let s = '';
      while (j < src.length && src[j] !== c) {
        if (src[j] === '\\' && j + 1 < src.length) { s += src[j + 1]; j += 2; continue; }
        s += src[j];
        j++;
      }
      if (j >= src.length) throw new Error('unterminated string');
      out.push({ t: 'str', v: s });
      i = j + 1;
      continue;
    }
    if (/[0-9]/.test(c) || (c === '-' && /[0-9]/.test(src[i + 1] ?? ''))) {
      const m = /^-?[0-9]+(\.[0-9]+)?/.exec(src.slice(i))!;
      out.push({ t: 'num', v: Number(m[0]) });
      i += m[0].length;
      continue;
    }
    if (/[a-z_]/.test(c)) {
      const m = /^[a-z_][a-z0-9_]*(\.[a-z_][a-z0-9_]*)*/.exec(src.slice(i))!;
      out.push({ t: 'id', v: m[0] });
      i += m[0].length;
      continue;
    }
    const two = src.slice(i, i + 2);
    if (two === '==' || two === '!=' || two === '&&' || two === '||') { out.push({ t: 'op', v: two }); i += 2; continue; }
    if ('!()[],'.includes(c)) { out.push({ t: 'op', v: c }); i++; continue; }
    throw new Error(`unexpected ${JSON.stringify(c)}`);
  }
  return out;
}

class Parser {
  private i = 0;
  constructor(private readonly toks: Tok[]) {}
  private peek(): Tok | undefined { return this.toks[this.i]; }
  private isOp(v: string): boolean { const t = this.peek(); return !!t && t.t === 'op' && t.v === v; }
  private isWord(v: string): boolean { const t = this.peek(); return !!t && t.t === 'id' && t.v === v; }
  private expect(v: string): void { if (!this.isOp(v)) throw new Error(`expected ${v}`); this.i++; }
  done(): boolean { return this.i >= this.toks.length; }

  expr(d: number): WhenNode {
    if (d > WHEN_MAX_DEPTH) throw new Error('too deeply nested');
    let a = this.and(d);
    while (this.isWord('or') || this.isOp('||')) { this.i++; a = { k: 'or', a, b: this.and(d) }; }
    return a;
  }
  private and(d: number): WhenNode {
    let a = this.not(d);
    while (this.isWord('and') || this.isOp('&&')) { this.i++; a = { k: 'and', a, b: this.not(d) }; }
    return a;
  }
  private not(d: number): WhenNode {
    if (this.isWord('not') || this.isOp('!')) {
      if (d + 1 > WHEN_MAX_DEPTH) throw new Error('too deeply nested');
      this.i++;
      return { k: 'not', a: this.not(d + 1) };
    }
    return this.cmp(d);
  }
  private cmp(d: number): WhenNode {
    const a = this.atom(d);
    if (this.isOp('==') || this.isOp('!=')) {
      const op = (this.toks[this.i++] as { v: string }).v;
      return { k: op === '==' ? 'eq' : 'ne', a, b: this.atom(d) };
    }
    if (this.isWord('in')) { this.i++; return { k: 'in', a, list: this.list(), neg: false }; }
    if (this.isWord('not') && this.toks[this.i + 1]?.t === 'id' && this.toks[this.i + 1]!.v === 'in') {
      this.i += 2;
      return { k: 'in', a, list: this.list(), neg: true };
    }
    return a;
  }
  private literal(): WhenValue {
    const t = this.peek();
    if (!t) throw new Error('unexpected end');
    if (t.t === 'str' || t.t === 'num') { this.i++; return t.v; }
    if (t.t === 'id' && (t.v === 'true' || t.v === 'false' || t.v === 'null')) { this.i++; return t.v === 'null' ? null : t.v === 'true'; }
    throw new Error('expected a literal');
  }
  private list(): WhenValue[] {
    this.expect('[');
    const out: WhenValue[] = [];
    if (this.isOp(']')) { this.i++; return out; }
    for (;;) {
      out.push(this.literal());
      if (this.isOp(']')) { this.i++; return out; }
      this.expect(',');
    }
  }
  private atom(d: number): WhenNode {
    const t = this.peek();
    if (!t) throw new Error('unexpected end');
    if (t.t === 'op' && t.v === '(') {
      this.i++;
      const e = this.expr(d + 1);
      this.expect(')');
      return e;
    }
    if (t.t === 'id' && !['true', 'false', 'null', 'and', 'or', 'not', 'in'].includes(t.v)) { this.i++; return { k: 'path', p: t.v.split('.') }; }
    return { k: 'lit', v: this.literal() };
  }
}

/** Parse a condition, or say why not. */
export function parseWhen(src: string): { ok: true; node: WhenNode } | { ok: false; error: string } {
  if (typeof src !== 'string' || src.length > WHEN_MAX_LEN) return { ok: false, error: 'too long' };
  try {
    const toks = lex(src);
    if (!toks.length) return { ok: false, error: 'empty' };
    const p = new Parser(toks);
    const node = p.expr(0);
    if (!p.done()) return { ok: false, error: 'trailing input' };
    return { ok: true, node };
  } catch (e) {
    return { ok: false, error: (e as Error).message };
  }
}

/** A context value at a path (missing → null; objects and arrays are not values → null). */
export function lookup(ctx: unknown, path: string[]): WhenValue {
  let v: unknown = ctx;
  for (const k of path) {
    if (!v || typeof v !== 'object' || Array.isArray(v) || !Object.prototype.hasOwnProperty.call(v, k)) return null;
    v = (v as Record<string, unknown>)[k];
  }
  return typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean' ? v : null;
}

const truthy = (v: WhenValue): boolean => !(v === null || v === false || v === '' || v === 0);

function value(n: WhenNode, ctx: unknown): WhenValue {
  switch (n.k) {
    case 'lit': return n.v;
    case 'path': return lookup(ctx, n.p);
    default: return test(n, ctx);
  }
}

function test(n: WhenNode, ctx: unknown): boolean {
  switch (n.k) {
    case 'lit': case 'path': return truthy(value(n, ctx));
    case 'not': return !test(n.a, ctx);
    case 'and': return test(n.a, ctx) && test(n.b, ctx);
    case 'or': return test(n.a, ctx) || test(n.b, ctx);
    case 'eq': return value(n.a, ctx) === value(n.b, ctx);
    case 'ne': return value(n.a, ctx) !== value(n.b, ctx);
    case 'in': { const v = value(n.a, ctx); return n.list.some((x) => x === v) !== n.neg; }
  }
}

/**
 * Evaluate a condition against a context. No condition (undefined or "") is
 * true; a condition that does not parse is false (never shown), never thrown.
 */
export function evalWhen(src: string | undefined | null, ctx: unknown): boolean {
  if (src === undefined || src === null || src === '') return true;
  const p = parseWhen(src);
  return p.ok ? test(p.node, ctx) : false;
}

/**
 * Fill `{path}` placeholders in plugin text from the same context ("Claude
 * login needed on {machine.name}"). A missing value renders as "". Only
 * `{ident.ident…}` is a placeholder; anything else in braces stays as is.
 */
export function renderText(text: string, ctx: unknown): string {
  return text.replace(/\{([a-z_][a-z0-9_]*(?:\.[a-z_][a-z0-9_]*)*)\}/g, (_, p: string) => {
    const v = lookup(ctx, p.split('.'));
    return v === null ? '' : String(v);
  });
}
