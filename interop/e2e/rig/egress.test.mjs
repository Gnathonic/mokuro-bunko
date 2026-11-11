// What the rig's browser may reach: `node --test e2e/rig/` in interop/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { refusedError, routeFor } from './egress.mjs';

test('the loopback hosts go through, whatever the port or scheme', () => {
    for (const url of [
        'http://127.0.0.1:4173/',
        'http://localhost:9/_sim/token',
        'ws://127.0.0.1:1/',
        'http://[::1]:8080/x',
    ]) {
        assert.equal(routeFor(url), 'continue', url);
    }
});

test('the two Google scripts app.html loads are answered empty, by exact address only', () => {
    assert.equal(routeFor('https://apis.google.com/js/api.js'), 'stub');
    assert.equal(routeFor('https://accounts.google.com/gsi/client'), 'stub');
    // The query string is not part of the match; another path or host is.
    assert.equal(routeFor('https://apis.google.com/js/api.js?onload=x'), 'stub');
    assert.equal(routeFor('https://apis.google.com/js/platform.js'), 'refuse');
    assert.equal(routeFor('http://apis.google.com/js/api.js'), 'refuse');
});

test('everything else is refused: a third-party font, the public host, a real cloud', () => {
    for (const url of [
        'https://fonts.googleapis.com/css2?family=Noto+Sans+JP',
        'https://www.googleapis.com/drive/v3/files',
        'https://graph.microsoft.com/v1.0/me',
        'http://127.0.0.2/',
        'http://offbox.localhost/',
    ]) {
        assert.equal(routeFor(url), 'refuse', url);
    }
});

test('a spec that was refused a host fails, naming each host once', () => {
    assert.equal(refusedError([]), null);
    const err = refusedError(['fonts.googleapis.com', 'example.invalid', 'fonts.googleapis.com']);
    assert.match(err, /off this machine/);
    assert.match(err, /fonts\.googleapis\.com, example\.invalid$/);
});
