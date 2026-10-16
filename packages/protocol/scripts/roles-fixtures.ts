/** Regenerate fixtures/roles.json (`npm run fixtures`). The Rust protocol crate reads the same file. */
import { createHash } from 'node:crypto';
import { writeFileSync } from 'node:fs';
import {
  ROLE_ACCESS, ROLE_COLORS, ROLE_DELEGATES_MAX, ROLE_GLYPH_MAX, ROLE_INSTRUCTIONS_MAX_BYTES, ROLE_LIST_MAX, ROLE_MAX_DEPTH,
  ROLE_MAX_SUBAGENTS, ROLE_MODEL_MAX, ROLE_MODELS_MAX, ROLE_NAME_MAX, ROLE_PURPOSE_MAX, ROLE_SCHEMA, ROLE_TOOL_MAX, ROLE_TOOLS_MAX,
  canonicalRole, parseRole,
} from '../dist/roles.js';

const full = {
  schema: 1,
  id: 'reviewer',
  name: 'Reviewer',
  avatar: { glyph: 'R', color: 'teal' },
  purpose: 'Reviews diffs for correctness and risk; never edits.',
  instructions: 'Read the diff paths you are given.\nReport blocking issues first.\n',
  cli: 'codex',
  model: { codex: 'gpt-5-codex', claude: 'opus', cursor: 'auto' },
  access: 'read-only',
  tools: { allow: ['Read', 'Grep'], deny: ['Bash(git push*)'] },
  mcp: ['github'],
  skills: ['review-checklist'],
  secrets: ['GH_TOKEN'],
  delegates: ['explorer'],
  limits: { max_subagents: 3, max_depth: 1 },
};

const valid: { name: string; input: unknown }[] = [
  { name: 'the-smallest-role', input: { id: 'ab', name: 'A' } },
  { name: 'every-field', input: full },
  { name: 'every-field-in-another-order', input: Object.fromEntries(Object.entries(full).reverse()) },
  { name: 'defaults-written-out-and-empty-parts', input: { schema: 1, id: 'ab', name: 'A', purpose: '', instructions: '', cli: 'any', model: {}, access: 'read-only', tools: { allow: [], deny: [] }, mcp: [], skills: [], secrets: [], delegates: [], limits: {} } },
  { name: 'text-that-needs-escaping', input: { id: 'quoter', name: 'Zoë "the" <b>Builder</b>', avatar: { glyph: '🛠', color: 'amber' }, purpose: 'Back\\slash, quote " and é', instructions: 'Tab\there\nnew line separator — done', access: 'edit+shell' } },
  { name: 'only-a-deny-list-and-one-limit', input: { id: 'tester', name: 'Tester', cli: 'claude', access: 'edit+shell', tools: { deny: ['Bash(rm -rf*)'] }, limits: { max_depth: 0 } } },
  { name: 'a-cli-this-build-has-never-heard-of', input: { id: 'pilot', name: 'Pilot', cli: 'some-new-cli', model: { 'some-new-cli': 'big-1' }, access: 'full' } },
];

const invalid: { name: string; input: unknown }[] = [
  { name: 'not-a-table', input: ['reviewer'] },
  { name: 'nothing-in-it', input: {} },
  { name: 'unknown-fields', input: { id: 'ab', name: 'A', colour: 'red', avatar: { glyph: 'A', color: 'red', size: 2 }, tools: { ask: [] }, limits: { max_width: 1 } } },
  { name: 'a-newer-schema', input: { schema: 2, id: 'ab', name: 'A' } },
  { name: 'bad-ids', input: { id: 'Reviewer', name: 'A', delegates: ['ok-role', 'Not_ok', 'ok-role'] } },
  { name: 'delegates-to-itself', input: { id: 'loop', name: 'Loop', delegates: ['loop'] } },
  { name: 'name-and-purpose-with-a-line-break', input: { id: 'ab', name: 'A\nB', purpose: 'one\ntwo' } },
  { name: 'name-too-long', input: { id: 'ab', name: 'é'.repeat(ROLE_NAME_MAX + 1) } },
  { name: 'name-ending-in-a-space', input: { id: 'ab', name: 'A ' } },
  { name: 'instructions-too-large', input: { id: 'ab', name: 'A', instructions: 'é'.repeat(ROLE_INSTRUCTIONS_MAX_BYTES / 2 + 1) } },
  { name: 'instructions-with-an-escape-character', input: { id: 'ab', name: 'A', instructions: 'red \u001b[31m text' } },
  { name: 'bad-avatar', input: { id: 'ab', name: 'A', avatar: { glyph: 'ABC', color: 'pink' } } },
  { name: 'bad-cli-model-and-access', input: { id: 'ab', name: 'A', cli: 'Claude Code', model: { any: 'x', claude: '', codex: 7 }, access: 'root' } },
  { name: 'wrong-types', input: { id: 7, name: null, purpose: 1, instructions: [], cli: {}, model: [], access: true, tools: 'Read', mcp: 'github', skills: {}, secrets: 1, delegates: 'x', limits: 3, avatar: 'R' } },
  { name: 'bad-list-entries', input: { id: 'ab', name: 'A', mcp: ['git hub'], skills: ['-x'], secrets: ['1TOKEN', 'OK_ONE'], tools: { allow: ['Read', 'Read', ''], deny: [3] } } },
  { name: 'lists-too-long', input: { id: 'ab', name: 'A', mcp: Array.from({ length: ROLE_LIST_MAX + 1 }, (_, i) => `m${i}`), delegates: Array.from({ length: ROLE_DELEGATES_MAX + 1 }, (_, i) => `role-${i}`) } },
  { name: 'limits-out-of-range', input: { id: 'ab', name: 'A', limits: { max_subagents: ROLE_MAX_SUBAGENTS + 1, max_depth: -1 } } },
  { name: 'limits-not-whole-numbers', input: { id: 'ab', name: 'A', limits: { max_subagents: 1.5, max_depth: '1' } } },
];

const out = {
  schema: ROLE_SCHEMA,
  access: ROLE_ACCESS,
  colors: ROLE_COLORS,
  limits: {
    name: ROLE_NAME_MAX, purpose: ROLE_PURPOSE_MAX, instructionsBytes: ROLE_INSTRUCTIONS_MAX_BYTES, glyph: ROLE_GLYPH_MAX, model: ROLE_MODEL_MAX,
    models: ROLE_MODELS_MAX, tool: ROLE_TOOL_MAX, tools: ROLE_TOOLS_MAX, list: ROLE_LIST_MAX, delegates: ROLE_DELEGATES_MAX,
    maxSubagents: ROLE_MAX_SUBAGENTS, maxDepth: ROLE_MAX_DEPTH,
  },
  valid: valid.map((c) => {
    const r = parseRole(c.input);
    if (!r.ok) throw new Error(`${c.name}: ${JSON.stringify(r.problems)}`);
    const canonical = canonicalRole(r.role);
    return { ...c, role: r.role, canonical, hash: createHash('sha256').update(canonical, 'utf8').digest('hex') };
  }),
  invalid: invalid.map((c) => {
    const r = parseRole(c.input);
    if (r.ok) throw new Error(`${c.name}: parsed`);
    return { ...c, paths: r.problems.map((p) => p.path) };
  }),
};
writeFileSync(new URL('../fixtures/roles.json', import.meta.url), JSON.stringify(out, null, 2) + '\n');
console.log(`roles.json: ${out.valid.length} valid, ${out.invalid.length} invalid`);
