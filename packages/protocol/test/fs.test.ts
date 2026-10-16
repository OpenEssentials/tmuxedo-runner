import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import {
  FS_EDITABLE_MAX, FS_LIST_MAX, FS_MAX_COMPONENTS, FS_MAX_COMPONENT_BYTES, FS_MAX_PATH_BYTES, FS_MAX_ROOT_ID, FS_MUTATING_METHODS,
  FS_READ_METHODS, FS_SEARCH_MAX_RESULTS, FS_SEARCH_QUERY_MAX, FS_TEXT_MAX, fsPathComponents, isFsMethod, parseFsCursor,
  parseFsReadParams, parseFsVersion, sameFsContent, validFsRootId,
  FS_WRITE_METHODS, FS_WEB_FLAGS, FS_STEPUP_WINDOW_MS, FS_STEPUP_SKEW_MS, FS_MAX_OP_ID, parseFsWriteParams, checkFsWebFlags,
} from '../src/fs.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/fs.json', import.meta.url), 'utf8'));

describe('fs.* contract (fixtures/fs.json, shared with the runner)', () => {
  it('methods and limits', () => {
    expect([...FS_READ_METHODS]).toEqual(fx.methods.read);
    expect([...FS_MUTATING_METHODS]).toEqual(fx.methods.mutating);
    for (const m of [...FS_READ_METHODS, ...FS_MUTATING_METHODS]) expect(isFsMethod(m)).toBe(true);
    expect(isFsMethod('files.list')).toBe(false);
    expect(isFsMethod('fs')).toBe(false);
    expect({
      pathBytes: FS_MAX_PATH_BYTES, components: FS_MAX_COMPONENTS, componentBytes: FS_MAX_COMPONENT_BYTES, rootId: FS_MAX_ROOT_ID,
      listPage: FS_LIST_MAX, searchQuery: FS_SEARCH_QUERY_MAX, searchResults: FS_SEARCH_MAX_RESULTS, textBytes: FS_TEXT_MAX, editableBytes: FS_EDITABLE_MAX,
    }).toEqual(fx.limits);
  });

  it('the path rule refuses and never cleans', () => {
    for (const p of fx.paths.valid) expect(fsPathComponents(p)?.join('/'), JSON.stringify(p)).toBe(p);
    for (const p of fx.paths.invalid) expect(fsPathComponents(p), JSON.stringify(p)).toBeNull();
    const lc = fx.paths.longComponent;
    expect(fsPathComponents(lc.char.repeat(lc.valid))).not.toBeNull();
    expect(fsPathComponents(lc.char.repeat(lc.invalid))).toBeNull();
    const mc = fx.paths.manyComponents;
    expect(fsPathComponents(Array(mc.valid).fill(mc.component).join('/'))).not.toBeNull();
    expect(fsPathComponents(Array(mc.invalid).fill(mc.component).join('/'))).toBeNull();
    const of = (n: number): string => {
      let s = '';
      while (s.length + 201 <= n) s += `${'x'.repeat(200)}/`;
      return s + 'y'.repeat(n - s.length);
    };
    expect(fsPathComponents(of(fx.paths.longPath.validBytes))).not.toBeNull();
    expect(fsPathComponents(of(fx.paths.longPath.invalidBytes))).toBeNull();
    // Bytes, not characters: 86 three-byte characters are 258 bytes.
    expect(fsPathComponents('日'.repeat(85))).not.toBeNull();
    expect(fsPathComponents('日'.repeat(86))).toBeNull();
    // Not a string, or a string with no UTF-8 form: never a path.
    for (const p of [null, undefined, 7, ['a'], { path: 'a' }, 'a\ud800', '\udc00b']) expect(fsPathComponents(p)).toBeNull();
  });

  it('root ids and cursors', () => {
    for (const id of fx.rootIds.valid) expect(validFsRootId(id), id).toBe(true);
    for (const id of fx.rootIds.invalid) expect(validFsRootId(id), JSON.stringify(id)).toBe(false);
    for (const [s, dir, name] of fx.cursors.valid) expect(parseFsCursor(s), s).toEqual({ dir, name });
    for (const s of fx.cursors.invalid) expect(parseFsCursor(s), JSON.stringify(s)).toBeNull();
    expect(parseFsCursor(`f/${'n'.repeat(255)}`)).not.toBeNull();
    expect(parseFsCursor(`f/${'n'.repeat(256)}`)).toBeNull();
  });

  it('hostile parameters are refused with the right code', () => {
    for (const row of fx.params) {
      const got = parseFsReadParams(row.method, row.params);
      if (row.code) {
        expect(got.ok, JSON.stringify(row)).toBe(false);
        if (!got.ok) expect(got.code, JSON.stringify(row)).toBe(row.code);
      } else {
        expect(got.ok, JSON.stringify(row)).toBe(true);
        if (got.ok && 'limit' in got.params) expect(got.params.limit).toBe(row.limit);
      }
    }
    // Inherited properties are not parameters.
    const proto = Object.create({ root: 'r1', path: 'a' }) as object;
    expect(parseFsReadParams('fs.read', proto).ok).toBe(false);
  });

  it('versions are strict', () => {
    for (const [s, size, mtimeNs] of fx.versions.valid) {
      const v = parseFsVersion(s);
      expect(v, s).not.toBeNull();
      expect(v!.size.toString()).toBe(size);
      expect(v!.mtimeNs.toString()).toBe(mtimeNs);
      expect(sameFsContent(v!, { ...v!, mtimeNs: 1n })).toBe(true);
    }
    for (const s of [...fx.versions.invalid, null, 5, {}]) expect(parseFsVersion(s), JSON.stringify(s)).toBeNull();
  });
});

describe('fs.* changing methods (slice A2, fixtures/fs.json write rows)', () => {
  it('methods, web flags and limits', () => {
    expect([...FS_WRITE_METHODS]).toEqual(fx.write.methods);
    for (const m of FS_WRITE_METHODS) expect(FS_MUTATING_METHODS).toContain(m);
    expect([...FS_WEB_FLAGS]).toEqual(fx.write.webFlags);
    expect([FS_STEPUP_WINDOW_MS, FS_STEPUP_SKEW_MS, FS_MAX_OP_ID]).toEqual([fx.write.stepupWindowMs, fx.write.stepupSkewMs, fx.write.opId]);
  });

  it('parameters are refused with the right code', () => {
    for (const row of fx.writeParams) {
      const got = parseFsWriteParams(row.method, row.params);
      if (row.code) {
        expect(got.ok, JSON.stringify(row)).toBe(false);
        if (!got.ok) expect(got.code, JSON.stringify(row)).toBe(row.code);
      } else {
        expect(got.ok, JSON.stringify(row)).toBe(true);
        if (got.ok && typeof row.create === 'boolean' && got.op.method === 'fs.write') expect(got.op.base === undefined).toBe(row.create);
      }
    }
    const big = parseFsWriteParams('fs.write', { root: 'r1', path: 'a', text: 'a'.repeat(FS_EDITABLE_MAX + 1), create: true, op_id: 'o' });
    expect(big.ok ? null : big.code).toBe('too_large');
    // Bytes, not characters: 'é' is two bytes.
    const wide = parseFsWriteParams('fs.write', { root: 'r1', path: 'a', text: 'é'.repeat(FS_EDITABLE_MAX / 2 + 1), create: true, op_id: 'o' });
    expect(wide.ok ? null : wide.code).toBe('too_large');
  });

  it('web flags decide control and step-up', () => {
    for (const row of fx.flagChecks) {
      const got = parseFsWriteParams('fs.undo', { ...row.flags, undoId: 'u0123456789abcdef', op_id: 'o' });
      expect(got.ok).toBe(true);
      if (got.ok) expect(checkFsWebFlags(got.flags, row.now), JSON.stringify(row)).toBe(row.code ?? null);
    }
  });
});
