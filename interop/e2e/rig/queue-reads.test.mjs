// Naming the queue files and marker the reader read, off its own requests:
// `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { namedInRequest, recordQueueReads, seriesFolders } from './queue-reads.mjs';

const SIM = 'http://127.0.0.1:4000';
const PREAUTH = 'http://localhost:4001';
const DAV = 'http://127.0.0.1:5000';

/** A page that hands each request to its listeners, and a rig whose tree is `rows`. */
function fakeBrowser(provider, rows = []) {
    const listeners = [];
    const page = {
        on: (event, fn) => {
            assert.equal(event, 'request');
            listeners.push(fn);
        },
        emit: (method, url, body = null) =>
            listeners.forEach((fn) => fn({ method: () => method, url: () => url, postData: () => body })),
    };
    const rig = {
        provider,
        cloudsimUrl: SIM,
        preauthUrl: PREAUTH,
        bunkoUrl: DAV,
        rcloneUrl: DAV,
        simAccount: 'bunko',
        sim: async (method, p) => {
            assert.equal(method, 'GET');
            assert.match(p, /^tree\?.*include_trashed=1/);
            return rows;
        },
    };
    return { page, rig };
}

const TREE = [
    { id: 'root', parent_id: null, name: 'mokuro-reader', folder: true },
    { id: 'f-a', parent_id: 'root', name: 'Example Saga', folder: true },
    { id: 'f-a2', parent_id: 'root', name: 'example saga', folder: true },
    { id: 'f-b', parent_id: 'root', name: 'Other Tale', folder: true },
    { id: 'q-a', parent_id: 'f-a', name: '.mokuro-series-queue.json' },
    { id: 'q-b', parent_id: 'f-b', name: '.mokuro-series-queue.json' },
    { id: 'm', parent_id: 'root', name: '.mokuro-bunko.json' },
    { id: 'v', parent_id: 'f-a', name: 'Vol 1.cbz' },
];

test('WebDAV: the path names the file and its series; other files and methods are no reads', async () => {
    for (const provider of ['local', 'rclone']) {
        const { page, rig } = fakeBrowser(provider);
        const reads = recordQueueReads(page, rig);
        page.emit('GET', `${DAV}/mokuro-reader/Example%20Saga/.mokuro-series-queue.json`);
        page.emit('GET', `${DAV}/mokuro-reader/.mokuro-queue.json`);
        page.emit('GET', `${DAV}/mokuro-reader/.mokuro-bunko.json`);
        page.emit('OPTIONS', `${DAV}/mokuro-reader/Other%20Tale/.mokuro-series-queue.json`);
        page.emit('PUT', `${DAV}/mokuro-reader/Other%20Tale/.mokuro-series-queue.json`);
        page.emit('GET', `${DAV}/mokuro-reader/Example%20Saga/Vol%201.cbz`);
        page.emit('GET', `http://127.0.0.1:9/mokuro-reader/Other%20Tale/.mokuro-series-queue.json`);
        const got = await reads.reads();
        assert.deepEqual(
            got.map(({ file, folder }) => [file, folder]),
            [
                ['series', 'Example Saga'],
                ['root', null],
                ['marker', null],
            ],
        );
        assert.deepEqual(seriesFolders(got), ['Example Saga']);
    }
});

test('Drive: a list names its folders by id, a download its node; mark() slices', async () => {
    const { page, rig } = fakeBrowser('google', TREE);
    const reads = recordQueueReads(page, rig);
    const list = (q) => `${SIM}/google/drive/v3/files?q=${encodeURIComponent(q)}&fields=files(id)`;
    page.emit('GET', list(`name='.mokuro-bunko.json' and trashed=false and ('root' in parents)`));
    const mark = reads.mark();
    page.emit(
        'GET',
        list(`name='.mokuro-series-queue.json' and trashed=false and ('f-a' in parents or 'f-a2' in parents)`),
    );
    page.emit('GET', `${SIM}/google/drive/v3/files/q-a?alt=media`);
    page.emit('GET', `${SIM}/google/drive/v3/files/v?alt=media`);
    page.emit('GET', list(`'me' in owners and trashed=false`));
    assert.deepEqual(
        (await reads.reads()).map(({ file, folder }) => [file, folder]),
        [
            ['marker', null],
            ['series', 'Example Saga'],
            ['series', 'example saga'],
            ['series', 'Example Saga'],
        ],
    );
    assert.deepEqual(seriesFolders(await reads.reads(mark)), ['Example Saga', 'example saga']);
});

test('Graph: the path names it, a download its item, the pre-authenticated URL too', async () => {
    const { page, rig } = fakeBrowser('microsoft', TREE);
    const reads = recordQueueReads(page, rig);
    page.emit('GET', `${SIM}/graph/v1.0/me/drive/root:/mokuro-reader/Other%20Tale/.mokuro-series-queue.json`);
    page.emit('GET', `${SIM}/graph/v1.0/me/drive/items/q-b/content`);
    page.emit('GET', `${PREAUTH}/graph/preauth/bunko/q-b`);
    page.emit('GET', `${SIM}/graph/v1.0/me/drive/root:/mokuro-reader/.mokuro-queue.json`);
    page.emit('GET', `${SIM}/graph/v1.0/me/drive/root:/mokuro-reader:/children`);
    assert.deepEqual(
        (await reads.reads()).map(({ file, folder }) => [file, folder]),
        [
            ['series', 'Other Tale'],
            ['series', 'Other Tale'],
            ['series', 'Other Tale'],
            ['root', null],
        ],
    );
});

test('MEGA: a download is a g command by handle; anything else is no read', async () => {
    const { page, rig } = fakeBrowser('mega', TREE);
    const reads = recordQueueReads(page, rig);
    page.emit('POST', `${SIM}/mega/cs?id=1`, JSON.stringify([{ a: 'g', g: 1, n: 'q-b' }, { a: 'f', c: 1 }]));
    page.emit('POST', `${SIM}/mega/cs?id=2`, JSON.stringify([{ a: 'g', g: 1, n: 'm' }]));
    page.emit('POST', `${SIM}/mega/cs?id=3`, 'not json');
    assert.deepEqual(
        (await reads.reads()).map(({ file, folder }) => [file, folder]),
        [
            ['series', 'Other Tale'],
            ['marker', null],
        ],
    );
});

test('namedInRequest leaves Drive query quoting as the reader escapes it', () => {
    const q = `name='it\\'s.json' and ('a\\'b' in parents)`;
    assert.deepEqual(
        namedInRequest({ provider: 'google', method: 'GET', url: `${SIM}/google/drive/v3/files?q=${encodeURIComponent(q)}` }),
        { names: [{ name: "it's.json", folder: { ids: ["a'b"] } }], nodes: [] },
    );
});
