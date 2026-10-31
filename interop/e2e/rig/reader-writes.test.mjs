// Naming what the reader wrote off its own requests: `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { recordReaderWrites } from './reader-writes.mjs';

const SIM = 'http://127.0.0.1:4000';
const PREAUTH = 'http://localhost:4000';

/** A page that hands each event to its listeners, and a rig whose tree is `rows`. */
function fakeBrowser(provider, rows = []) {
    const listeners = new Map();
    const page = {
        on: (event, fn) => listeners.set(event, [...(listeners.get(event) ?? []), fn]),
        emit: (event, request) => (listeners.get(event) ?? []).forEach((fn) => fn(request)),
    };
    const rig = {
        provider,
        cloudsimUrl: SIM,
        preauthUrl: PREAUTH,
        simAccount: 'bunko',
        sim: async (method, p) => {
            assert.equal(method, 'GET');
            assert.match(p, /^tree\?/);
            return rows;
        },
    };
    return { page, rig };
}

/**
 * A request as Playwright hands it over. `body` is what it sent (null: none), `answer`
 * its response (null: it got none), whose `json` body is unavailable when `json` is
 * undefined (as for a Worker gone by the time the body is asked for).
 */
function request(method, url, { body = null, answer = { status: 200 } } = {}) {
    const response = answer && {
        status: () => answer.status,
        headerValue: async (name) => answer.headers?.[name] ?? null,
        allHeaders: async () => answer.headers ?? {},
        body: async () => {
            if (answer.json === undefined) throw new Error('response.body: Response body is unavailable');
            return Buffer.from(JSON.stringify(answer.json));
        },
    };
    const sent = body === null ? null : Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
    return {
        url: () => url,
        method: () => method,
        postDataBuffer: () => sent,
        postData: () => sent?.toString('utf8') ?? null,
        allHeaders: async () => ({}),
        response: async () => response,
    };
}

const SESSION = `${SIM}/google/upload/drive/v3/files?uploadType=resumable&upload_id=62756e6b6f.s00453`;

test("a Worker's Drive upload is named by its session: the init failed late, the PUT's answer is gone", async () => {
    const { page, rig } = fakeBrowser('google');
    const writes = recordReaderWrites(page, rig);
    // The Worker read only the init's Location, so Chromium reports it failed.
    page.emit(
        'requestfailed',
        request('POST', `${SIM}/google/upload/drive/v3/files?uploadType=resumable&fields=id`, {
            body: { name: 'Vol 1.cbz', mimeType: 'application/x-cbz', parents: ['f00448'] },
            answer: { status: 200, headers: { location: SESSION } },
        }),
    );
    page.emit('requestfinished', request('PUT', SESSION, { answer: { status: 200 } }));
    assert.deepEqual(await writes.names(), ['Vol 1.cbz']);
    assert.deepEqual(writes.unnamed(), []);
});

test("a Drive update session names its file by the id in the init's path", async () => {
    const { page, rig } = fakeBrowser('google', [{ id: 'g00128', name: 'catalog.json' }]);
    const writes = recordReaderWrites(page, rig);
    const session = `${SIM}/google/upload/drive/v3/files?uploadType=resumable&upload_id=62756e6b6f.s00459`;
    // The content PUT may be looked at before the init it follows.
    page.emit('requestfinished', request('PUT', session, { answer: { status: 200 } }));
    page.emit(
        'requestfinished',
        request('PATCH', `${SIM}/google/upload/drive/v3/files/g00128?uploadType=resumable`, {
            body: { mimeType: 'application/json' },
            answer: { status: 200, headers: { location: session } },
        }),
    );
    assert.deepEqual(await writes.names(), ['catalog.json']);
    assert.deepEqual(writes.unnamed(), []);
});

test('an upload session that wrote nothing names nothing', async () => {
    const { page, rig } = fakeBrowser('google');
    const writes = recordReaderWrites(page, rig);
    page.emit(
        'requestfinished',
        request('POST', `${SIM}/google/upload/drive/v3/files?uploadType=resumable`, {
            body: { name: 'never.json' },
            answer: { status: 200, headers: { location: SESSION } },
        }),
    );
    // Its PUT never got an answer.
    page.emit('requestfailed', request('PUT', SESSION, { answer: null }));
    assert.deepEqual(await writes.names(), []);
    assert.deepEqual(writes.unnamed(), []);
});

test('a session PUT whose init was never seen stays unnamed', async () => {
    const { page, rig } = fakeBrowser('google');
    const writes = recordReaderWrites(page, rig);
    page.emit('requestfinished', request('PUT', SESSION, { answer: { status: 200 } }));
    assert.deepEqual(await writes.names(), []);
    assert.deepEqual(writes.unnamed(), ['PUT /google/upload/drive/v3/files']);
});

test("a OneDrive upload is named by its session, its fragments' answers no help", async () => {
    const { page, rig } = fakeBrowser('microsoft');
    const writes = recordReaderWrites(page, rig);
    const uploadUrl = `${PREAUTH}/graph/upload/626b/s7`;
    page.emit(
        'requestfinished',
        request('POST', `${SIM}/graph/v1.0/me/drive/root:/mokuro-reader/S/Vol%201.cbz:/createUploadSession`, {
            body: { item: { '@microsoft.graph.conflictBehavior': 'replace' } },
            answer: { status: 200, json: { uploadUrl, nextExpectedRanges: ['0-'] } },
        }),
    );
    // A fragment answers 202 with no item; the last one's answer is gone.
    page.emit(
        'requestfinished',
        request('PUT', uploadUrl, { answer: { status: 202, json: { nextExpectedRanges: ['327680-'] } } }),
    );
    page.emit('requestfinished', request('PUT', uploadUrl, { answer: { status: 201 } }));
    assert.deepEqual(await writes.names(), ['Vol 1.cbz']);
    assert.deepEqual(writes.unnamed(), []);
});

test('a OneDrive session whose answer is gone still names its file by its path', async () => {
    const { page, rig } = fakeBrowser('microsoft');
    const writes = recordReaderWrites(page, rig);
    page.emit(
        'requestfinished',
        request('POST', `${SIM}/graph/v1.0/me/drive/root:/mokuro-reader/positions.json:/createUploadSession`, {
            body: { item: {} },
            answer: { status: 200 },
        }),
    );
    assert.deepEqual(await writes.names(), ['positions.json']);
});

test('a write that got no answer wrote nothing; reads are no writes', async () => {
    const { page, rig } = fakeBrowser('rclone');
    rig.rcloneUrl = 'http://127.0.0.1:5000';
    const writes = recordReaderWrites(page, rig);
    page.emit('requestfailed', request('PUT', `${rig.rcloneUrl}/mokuro-reader/lost.json`, { answer: null }));
    page.emit('requestfinished', request('PROPFIND', `${rig.rcloneUrl}/mokuro-reader/`));
    page.emit('requestfinished', request('PUT', `${rig.rcloneUrl}/mokuro-reader/positions.json`));
    // Another origin is not the cloud.
    page.emit('requestfinished', request('PUT', 'http://127.0.0.1:6000/mokuro-reader/x.json'));
    assert.deepEqual(await writes.names(), ['positions.json']);
    assert.deepEqual(writes.unnamed(), []);
});

test("a session whose opening named nothing leaves its content writes unnamed, not dropped", async () => {
    const { page, rig } = fakeBrowser('microsoft');
    const writes = recordReaderWrites(page, rig);
    const uploadUrl = `${PREAUTH}/graph/upload/626b/s9`;
    // Opened by item id: no name in the path, none in the answer.
    page.emit(
        'requestfinished',
        request('POST', `${SIM}/graph/v1.0/me/drive/items/o00042/createUploadSession`, {
            body: { item: {} },
            answer: { status: 200, json: { uploadUrl } },
        }),
    );
    page.emit('requestfinished', request('PUT', uploadUrl, { answer: { status: 201 } }));
    assert.deepEqual(await writes.names(), []);
    assert.deepEqual(writes.unnamed(), ['PUT /graph/upload/626b/s9']);
});
