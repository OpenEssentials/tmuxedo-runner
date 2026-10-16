import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import {
  FEATURE_REVIEW, REVIEW_LARGE_LINES, REVIEW_MAX_SESSION_ID, REVIEW_MUTATING_METHODS, REVIEW_PATCH_MAX, REVIEW_READ_METHODS,
  REVIEW_STATUS_FILES_MAX, isReviewMethod, parseReviewReadParams,
} from '../src/review.js';

const fx = JSON.parse(readFileSync(new URL('../fixtures/review.json', import.meta.url), 'utf8'));

describe('git.* contract (fixtures/review.json, shared with the runner)', () => {
  it('feature, methods and limits', () => {
    expect(FEATURE_REVIEW).toBe(fx.feature);
    expect([...REVIEW_READ_METHODS]).toEqual(fx.methods.read);
    expect([...REVIEW_MUTATING_METHODS]).toEqual(fx.methods.mutating);
    for (const m of [...REVIEW_READ_METHODS, ...REVIEW_MUTATING_METHODS]) expect(isReviewMethod(m)).toBe(true);
    expect(isReviewMethod('fs.read')).toBe(false);
    expect(isReviewMethod('git')).toBe(false);
    expect({ sessionId: REVIEW_MAX_SESSION_ID, statusFiles: REVIEW_STATUS_FILES_MAX, patchBytes: REVIEW_PATCH_MAX, largeLines: REVIEW_LARGE_LINES }).toEqual(fx.limits);
  });

  it('parameter shapes', () => {
    for (const row of fx.params.valid) expect(parseReviewReadParams(row.method, row.params).ok, JSON.stringify(row)).toBe(true);
    for (const row of fx.params.invalid) {
      const r = parseReviewReadParams(row.method, row.params);
      expect(r.ok, JSON.stringify(row)).toBe(false);
      if (!r.ok) expect(r.code, JSON.stringify(row)).toBe(row.code);
    }
  });
});
