/**
 * The `when` evaluator and `{path}` text against fixtures/when.json, the same
 * file the Rust evaluator (runner/crates/protocol/src/when.rs) is tested on.
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { evalWhen, parseWhen, renderText, WHEN_MAX_LEN } from '../src/when.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/when.json', import.meta.url), 'utf8')) as {
  ctx: unknown; cases: Array<{ expr: string; want: boolean }>; empty_is_true: Array<string | null>;
  invalid: string[]; too_long: number; text: Array<{ text: string; want: string }>;
};

describe('when (shared fixtures)', () => {
  it.each(fx.cases)('$expr → $want', ({ expr, want }) => {
    expect(parseWhen(expr).ok).toBe(true);
    expect(evalWhen(expr, fx.ctx)).toBe(want);
  });

  it.each(fx.invalid)('refuses %s', (expr) => {
    expect(parseWhen(expr).ok).toBe(false);
    // A condition that does not parse never shows its surface.
    expect(evalWhen(expr, fx.ctx)).toBe(false);
  });

  it('no condition is true; too long is refused', () => {
    for (const e of fx.empty_is_true) expect(evalWhen(e, fx.ctx)).toBe(true);
    expect(fx.too_long).toBe(WHEN_MAX_LEN + 1);
    expect(parseWhen('x'.repeat(fx.too_long)).ok).toBe(false);
    expect(parseWhen('x'.repeat(WHEN_MAX_LEN)).ok).toBe(true);
  });

  it.each(fx.text)('text $text', ({ text, want }) => {
    expect(renderText(text, fx.ctx)).toBe(want);
  });

  it('never throws on junk contexts', () => {
    for (const ctx of [null, undefined, 3, 'x', [], { auth: [] }, { auth: { state: { deep: 1 } } }]) {
      expect(evalWhen("auth.state == 'ok'", ctx)).toBe(false);
      expect(evalWhen('auth.state == null', ctx)).toBe(true);
      expect(renderText('{auth.state}', ctx)).toBe('');
    }
  });

  it('a prototype key is not a context value', () => {
    expect(evalWhen('auth.constructor', { auth: {} })).toBe(false);
    expect(evalWhen('auth.to_string == null', { auth: {} })).toBe(true);
  });
});
