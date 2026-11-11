// The capability gate as a pure function: `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { gateCapabilities, parsePendingCaps } from './caps.mjs';

test('the pending list is one capability per line, # comments and blanks ignored', () => {
    assert.deepEqual(parsePendingCaps(''), []);
    assert.deepEqual(
        parsePendingCaps('# header\n\nhistory\n  dedup  # until reader.ref has it\r\n#positions\n'),
        ['history', 'dedup'],
    );
});

test('a malformed or repeated capability in the pending list is an error, not a silent skip', () => {
    assert.throws(() => parsePendingCaps('edit layers\n'), /line 1/);
    assert.throws(() => parsePendingCaps('History\n'), /line 1/);
    assert.throws(() => parsePendingCaps('history\nhistory\n'), /line 2.*twice/);
});

test('a spec runs when the reader has every capability it needs', () => {
    assert.deepEqual(gateCapabilities(['history'], ['history', 'dedup'], []), {
        action: 'run',
        missing: [],
        unlisted: [],
    });
    assert.equal(gateCapabilities([], [], []).action, 'run');
});

test('a missing capability fails the spec unless the pending list names it', () => {
    assert.deepEqual(gateCapabilities(['history'], [], []), {
        action: 'fail',
        missing: ['history'],
        unlisted: ['history'],
    });
    assert.deepEqual(gateCapabilities(['history'], [], ['history']), {
        action: 'skip',
        missing: ['history'],
        unlisted: [],
    });
});

test('one unlisted capability among pending ones still fails', () => {
    assert.deepEqual(gateCapabilities(['history', 'dedup', 'provenance'], ['provenance'], ['dedup']), {
        action: 'fail',
        missing: ['history', 'dedup'],
        unlisted: ['history'],
    });
});

test('a pending capability the reader already has does not skip the spec', () => {
    assert.equal(gateCapabilities(['history'], ['history'], ['history']).action, 'run');
});
