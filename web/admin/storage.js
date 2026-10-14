// The Storage tab (0.8 §11): where the library lives and how its store is doing,
// from `/_admin/api/storage` (the same block as `/api/status` → library.storage).
// Actions: Connect (one button; the server picks the sign-in flow: the browser here,
// a code, or for OneDrive elsewhere the address the browser ends on, pasted), the account
// choice, "Create it?", Sign in again, Change pairing (a Disconnect, then a Connect
// with the other reader), Disconnect (Wait / Export / Discard), Take over, readers,
// "Keep this copy" / "Keep that copy" of a blocked case merge; "Recent merges".
// Nothing here ever sees a token or a password beyond the form it was typed into.
'use strict';

let storageTimer = null;
let storageState = null;
let storageUsersLoaded = false;

const STORAGE_NAMES = {
    local: 'This computer', gdrive: 'Google Drive', onedrive: 'OneDrive', mega: 'MEGA', webdav: 'WebDAV server',
};

function stEl(id) { return document.getElementById(id); }

function stText(id, text) {
    const el = stEl(id);
    if (el) el.textContent = text == null || text === '' ? '-' : String(text);
}

function stPairing(p) {
    if (p === 'release') return 'reader.mokuro.app (release)';
    if (p === 'beta') return 'Mokuro Reader beta';
    if (p && p.startsWith('custom:')) return 'custom (' + p.slice(7) + ')';
    return p || '';
}

function stAgo(unixSeconds) {
    if (!unixSeconds) return '';
    const s = Math.max(0, Math.round(Date.now() / 1000 - unixSeconds));
    if (s < 60) return 'just now';
    if (s < 3600) return Math.round(s / 60) + ' min ago';
    if (s < 86400) return Math.round(s / 3600) + ' h ago';
    return Math.round(s / 86400) + ' days ago';
}

function stHealth(st) {
    const h = st.health || {};
    switch (h.state) {
        case 'online': return 'In sync' + (st.synced_at ? ' (checked ' + stAgo(st.synced_at) + ')' : '');
        case 'syncing': return 'Syncing' + (h.total ? ' ' + h.done + '/' + h.total : h.done ? ' (' + h.done + ' listed)' : '');
        case 'degraded': return 'Slowed down by the provider, until ' + new Date(h.retry_at * 1000).toLocaleTimeString();
        case 'offline': return 'Offline: retrying' + (st.synced_at ? ' (last in sync ' + stAgo(st.synced_at) + ')' : '');
        case 'needs-reauth': return st.connected ? 'Needs sign-in' : 'Not connected';
        case 'blocked': return 'Paused: ' + (h.reason || 'quota');
        case 'suspect': return 'Holding removals (many files disappeared at once)';
        case 'lease-held': return 'Standby: ' + (h.by || 'another Bunko') + ' manages this library';
        default: return h.state || '-';
    }
}

function startStoragePolling() {
    stopStoragePolling();
    const tick = async () => {
        await loadStorage();
        const busy = storageState && storageState.connect && ['starting', 'browser', 'paste', 'code', 'checking'].includes(storageState.connect.state);
        storageTimer = setTimeout(tick, busy ? 1500 : 10000);
    };
    storageTimer = setTimeout(tick, 1500);
}

function stopStoragePolling() {
    if (storageTimer) clearTimeout(storageTimer);
    storageTimer = null;
}

async function loadStorage() {
    let st;
    try {
        st = await apiGet('/storage');
    } catch (err) {
        if (err.status !== 404) showToast('Storage: ' + err.message, 'error');
        return;
    }
    storageState = st;
    const cloud = st.backend !== 'local';
    stText('st-provider', STORAGE_NAMES[st.backend] || st.provider);
    stText('st-account', cloud ? (st.account || 'not signed in') : '');
    stText('st-folder', st.folder);
    stEl('st-pairing-row').hidden = !st.pairing;
    stText('st-pairing', stPairing(st.pairing) + (st.pairing_change ? ' — configured ' + stPairing(st.pairing_change.configured) + ', connected ' + stPairing(st.pairing_change.connected) : ''));
    stText('st-health', stHealth(st));
    const sync = st.sync;
    stText('st-sync', st.engine === 'steady' ? 'Up to date' : st.engine === 'full-sync' && sync
        ? 'First sync: ' + (sync.total ? sync.done + ' of ' + sync.total : sync.done + ' listed') : st.engine || '-');
    stEl('st-store-row').hidden = !st.store_id;
    stText('st-store', st.store_id);
    const q = st.quota;
    stEl('st-quota-row').hidden = !q || q.total == null;
    if (q && q.total != null) stText('st-quota', formatBytes(q.used) + ' of ' + formatBytes(q.total) + ' (' + formatBytes(q.available) + ' free)');
    // Problems (storage-*).
    const box = stEl('storage-problems');
    box.replaceChildren(...(st.problems || []).map((p) => {
        const el = document.createElement('div');
        el.className = 'alert ' + (p.severity === 'fail' ? 'alert--error' : 'alert--warning') + ' mb-3';
        el.textContent = p.text + (p.hint ? ' — ' + p.hint : '');
        return el;
    }));
    stEl('storage-restart').hidden = !st.restart_pending;
    // Actions.
    const needsAuth = cloud && st.health && st.health.state === 'needs-reauth';
    stEl('st-reauth').hidden = !needsAuth || !!st.pairing_change;
    stEl('st-disconnect').hidden = !cloud;
    stEl('st-pairing-change').hidden = !(st.backend === 'gdrive' && st.pairing && !st.pairing.startsWith('custom'));
    stEl('st-change').textContent = cloud ? 'Use another library…' : 'Keep the library in the cloud…';
    // Readers.
    stEl('sr-serve').checked = !!st.serve_readers;
    stEl('sr-progress-row').hidden = !(cloud && st.serve_readers);
    if (cloud && st.serve_readers) await loadStorageUsers(st.progress_user);
    // Outbox, caches, conflicts.
    const ob = st.outbox;
    stEl('storage-outbox').hidden = !ob;
    if (ob) {
        stText('so-owed', ob.owed + (ob.uploading ? ' (' + ob.uploading + ' uploading now)' : ''));
        stText('so-oldest', ob.oldest ? ob.oldest.path + ' (' + ob.oldest.state.toLowerCase() + ', ' + ob.oldest.attempts + ' tries)' : '');
        stText('so-error', ob.last_error);
        stText('so-held', ob.held + ' / ' + ob.failed);
        const ops = stEl('so-ops');
        ops.replaceChildren(...[...(ob.failed_ops || []), ...(ob.held_ops || [])].slice(0, 20).map((o) => {
            const li = document.createElement('li');
            li.textContent = o.state + ' ' + o.path + (o.last_error ? ': ' + o.last_error : '');
            return li;
        }));
        const c = st.cache || {};
        stText('so-cache', (c.blobs || 0) + ' small files (' + formatBytes(c.blob_bytes || 0) + '), ' + (c.zipdirs || 0) + ' archive directories');
        stText('so-spool', (c.spools || 0) + ' archives (' + formatBytes(c.spool_bytes || 0) + ')');
        const sub = st.submissions || {};
        stText('so-submissions', sub.waiting || 0);
        const conflicts = st.conflicts || [];
        stEl('so-conflicts-none').hidden = conflicts.length > 0;
        stEl('so-conflicts-hint').hidden = conflicts.length === 0;
        stEl('so-conflicts').replaceChildren(...conflicts.map((k) => {
            const li = document.createElement('li');
            li.textContent = k.kind + ': ' + k.path + (k.detail ? ' — ' + k.detail : '');
            if (k.resolvable) {
                const row = document.createElement('div');
                row.className = 'connectivity-section__actions';
                const keep = (choice, paths) => {
                    const b = document.createElement('button');
                    b.className = 'btn btn--secondary btn--sm';
                    b.textContent = (choice === 'this' ? 'Keep this copy' : 'Keep that copy') + ' (' + paths.join(', ') + ')';
                    b.onclick = () => storageKeepCopy(k, choice);
                    return b;
                };
                row.append(keep('this', k.this || []), keep('that', k.that || []));
                li.appendChild(row);
            }
            return li;
        }));
        const merges = st.recent_merges || [];
        stEl('so-merges-none').hidden = merges.length > 0;
        stEl('so-merges').replaceChildren(...merges.map((m) => {
            const li = document.createElement('li');
            const when = stAgo((m.finished_at || m.created_at) / 1000);
            const what = m.origin === 'admin' ? 'Kept one copy in ' + m.winner : (m.losers || []).join(', ') + ' → ' + m.winner;
            const counts = m.moved + ' moved, ' + m.trashed + ' to the trash' + (m.blocked ? ', ' + m.blocked + ' left (see Conflicts)' : '');
            li.textContent = what + ': ' + (m.state === 'done' ? counts : 'in progress (' + counts + ')') + (when ? ' · ' + when : '');
            return li;
        }));
    }
    // Lease.
    const lease = st.lease;
    stEl('storage-lease').hidden = !lease;
    if (lease) {
        stText('sl-state', {
            'not-started': 'Starts after the first sync',
            claiming: lease.detail ? lease.detail + ' (uploads wait until then)' : 'Claiming: uploads wait until no other Bunko answers (the first claim takes a few minutes)',
            held: 'Held by this Bunko',
            standby: 'Standby: another Bunko manages this library',
            lost: 'Lost to another Bunko',
            'heartbeat-failed': 'Heartbeat failed, retrying' + (lease.error ? ': ' + lease.error : ''),
            released: 'Released',
        }[lease.state] || lease.state);
        const h = lease.holder || {};
        stText('sl-holder', h.ours ? 'this Bunko' : (h.name || h.instance_id || ''));
        stText('sl-epoch', h.epoch);
        stText('sl-seen', h.last_seen);
        stEl('sl-takeover').hidden = !lease.can_take_over;
    }
    renderStorageConnect(st.connect || {}, st);
}

async function loadStorageUsers(selected) {
    const sel = stEl('sr-progress');
    if (!storageUsersLoaded) {
        try {
            const data = await apiGet('/users');
            const names = (data.users || []).filter((u) => u.status !== 'deleted' && u.role !== 'processor').map((u) => u.username);
            sel.replaceChildren(new Option('(nobody)', ''), ...names.map((n) => new Option(n, n)));
            storageUsersLoaded = true;
        } catch (_) { /* leave it */ }
    }
    sel.value = selected || '';
}

async function saveStorageReaders() {
    const serve = stEl('sr-serve').checked;
    const user = stEl('sr-progress').value || null;
    try {
        await apiPut('/storage/readers', { serve_readers: serve, progress_user: serve ? user : null });
        showToast(serve ? 'Readers connect to this Bunko.' : 'Readers keep reading from the cloud; this Bunko serves none.', 'success');
        loadStorage();
    } catch (err) {
        showToast(err.message, 'error');
    }
}

// ---- Connect ------------------------------------------------------------------------

function storageShowConnect(reauth) {
    const st = storageState || {};
    stEl('storage-connect').hidden = false;
    const conf = st.configured || {};
    if (reauth && st.backend && st.backend !== 'local') {
        stEl('sc-provider').value = st.backend;
    } else if (conf.backend) {
        stEl('sc-provider').value = conf.backend;
    }
    if (conf.google_project && conf.google_project !== 'custom') stEl('sc-reader-select').value = conf.google_project;
    if (conf.webdav) {
        if (conf.webdav.url) stEl('sc-dav-url').value = conf.webdav.url;
        if (conf.webdav.username) stEl('sc-dav-user').value = conf.webdav.username;
        stEl('sc-dav-http').checked = !!conf.webdav.allow_http;
    }
    storageConnectForm();
    stEl('storage-connect').scrollIntoView({ behavior: 'smooth', block: 'start' });
}

function storageConnectForm() {
    const p = stEl('sc-provider').value;
    const conf = (storageState && storageState.configured) || {};
    const custom = p === 'gdrive' && conf.custom_pairing;
    stEl('sc-reader').hidden = p !== 'gdrive' || custom;
    stEl('sc-custom').hidden = !custom;
    if (custom) stEl('sc-custom').textContent = 'Pairing: ' + (conf.custom_pairing_label || 'custom') + ' (storage.library.oauth.google).';
    stEl('sc-mega').hidden = p !== 'mega';
    stEl('sc-webdav').hidden = p !== 'webdav';
    // OneDrive's code is for work or school accounts (personal ones cannot use one).
    stEl('sc-code').hidden = !(p === 'gdrive' || p === 'onedrive');
    stEl('sc-code').textContent = p === 'onedrive' ? 'Use a sign-in code (work or school)' : 'Use a code instead';
    stEl('sc-connect').textContent = p === 'local' ? 'Use this computer' : 'Connect';
}

const STORAGE_LOOPBACK = ['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);

// flow: null (the server chooses), 'device', 'paste' or 'loopback'.
async function storageConnect(flow) {
    const p = stEl('sc-provider').value;
    const st = storageState || {};
    if (p === 'local') {
        if (!confirm('Keep the library on this computer? This is a different library from a cloud one.')) return;
        try {
            const r = await apiPost('/storage/local', {});
            showToast(r.restarting ? 'The server restarts with the library on this computer.' : 'The library is on this computer.', 'success');
        } catch (err) { showToast(err.message, 'error'); }
        loadStorage();
        return;
    }
    if (st.backend === 'local' && !confirm('Keep the library in ' + STORAGE_NAMES[p] + ' instead? This is a different library: the one on this computer stays where it is.')) return;
    const body = { provider: p };
    if (p === 'gdrive' && !stEl('sc-reader').hidden) body.reader = stEl('sc-reader-select').value;
    if (p === 'mega') body.mega = { email: stEl('sc-mega-email').value.trim(), password: stEl('sc-mega-password').value, totp: stEl('sc-mega-totp').value.trim() || null };
    if (p === 'webdav') body.webdav = { url: stEl('sc-dav-url').value.trim(), username: stEl('sc-dav-user').value.trim(), password: stEl('sc-dav-password').value, allow_http: stEl('sc-dav-http').checked };
    if (flow) body.flow = flow;
    // The loopback sign-in's tab, opened while the click still counts (pop-up blockers).
    let tab = null;
    if ((p === 'gdrive' || p === 'onedrive') && STORAGE_LOOPBACK && flow !== 'device' && flow !== 'paste') tab = window.open('', '_blank');
    stEl('sc-paste-url').value = '';
    try {
        await apiPost('/storage/connect', body);
    } catch (err) {
        if (tab) tab.close();
        showToast(err.message, 'error');
        return;
    }
    stEl('sc-mega-password').value = '';
    stEl('sc-dav-password').value = '';
    storageFollowTab(tab);
    startStoragePolling();
}

// Point the waiting tab at the sign-in page once the server answers with one.
function storageFollowTab(tab) {
    if (!tab) return;
    const tick = async () => {
        let s;
        try { s = await apiGet('/storage/connect'); } catch (_) { tab.close(); return; }
        if (s.state === 'browser' && s.auth_url) { tab.location.href = s.auth_url; return; }
        if (['starting'].includes(s.state)) { setTimeout(tick, 500); return; }
        tab.close();
    };
    tick();
}

// The paste flow: the address the browser ended on after signing in elsewhere.
async function storagePaste() {
    const url = stEl('sc-paste-url').value.trim();
    if (!url) { showToast('Paste the address the browser ended on.', 'error'); return; }
    stEl('sc-paste-btn').disabled = true;
    try {
        await apiPost('/storage/connect/paste', { url });
        stEl('sc-paste-url').value = '';
        stEl('sc-paste').hidden = true;
        loadStorage();
    } catch (err) {
        showToast(err.message, 'error');
    } finally {
        stEl('sc-paste-btn').disabled = false;
    }
}

async function storageCancel() {
    try { await apiPost('/storage/connect/cancel', {}); } catch (_) { /* fine */ }
    stEl('storage-connect').hidden = true;
    loadStorage();
}

function renderStorageConnect(c, st) {
    const live = stEl('sc-live');
    // The paste field sits outside the live area, so a poll does not wipe it.
    stEl('sc-paste').hidden = !(c && c.state === 'paste');
    if (!c || c.state === 'idle') { live.hidden = true; return; }
    stEl('storage-connect').hidden = false;
    live.hidden = false;
    live.replaceChildren();
    const p = (text) => { const el = document.createElement('p'); el.textContent = text; live.appendChild(el); return el; };
    const name = STORAGE_NAMES[c.provider] || 'the provider';
    const link = (href, text) => { const a = document.createElement('a'); a.href = href; a.target = '_blank'; a.rel = 'noopener'; a.textContent = text; return a; };
    switch (c.state) {
        case 'starting': p('Starting the sign-in…'); break;
        case 'browser': p('Sign in to ' + name + ' in the tab that opened. '); live.appendChild(link(c.auth_url, 'Open the sign-in page')); break;
        case 'paste': {
            p('Sign in to ' + name + ' on any device, a phone included:');
            live.appendChild(link(c.auth_url, 'Open the sign-in page'));
            const left = c.expires_at ? Math.max(0, Math.round((new Date(c.expires_at) - Date.now()) / 60000)) : null;
            p('The browser then ends on a page that does not load (' + (c.redirect_uri || 'http://localhost') + '/?code=…). Paste its address below' + (left != null ? ' within about ' + left + ' min' : '') + '.');
            break;
        }
        case 'code': {
            const intro = p('Open ');
            intro.appendChild(link(c.verification_url, c.verification_url));
            intro.appendChild(document.createTextNode(' on any device and enter:'));
            const code = document.createElement('div');
            code.className = 'storage-code';
            code.textContent = c.user_code;
            live.appendChild(code);
            const left = c.expires_at ? Math.max(0, Math.round((new Date(c.expires_at) - Date.now()) / 60000)) : null;
            p('Code ' + c.code_number + ' of ' + c.max_codes + (left != null ? ' · expires in about ' + left + ' min' : '') + '; a new one follows by itself.');
            break;
        }
        case 'checking': p(c.message || 'Checking…'); break;
        case 'account-mismatch': {
            p(c.message || 'This is another account.');
            const use = document.createElement('button');
            use.className = 'btn btn--danger btn--sm';
            use.textContent = 'Use this account\'s library instead';
            use.onclick = async () => {
                try { await apiPost('/storage/connect/account', { use_this_account: true }); } catch (err) { showToast(err.message, 'error'); }
                loadStorage();
            };
            const no = document.createElement('button');
            no.className = 'btn btn--secondary btn--sm';
            no.textContent = 'Keep the library on file';
            no.onclick = async () => { await apiPost('/storage/connect/account', { use_this_account: false }).catch(() => {}); loadStorage(); };
            const row = document.createElement('div');
            row.className = 'connectivity-section__actions';
            row.append(use, no);
            live.appendChild(row);
            break;
        }
        case 'connected': {
            const ck = c.check || {};
            p('Connected as ' + (c.account || '?') + '. ' + (ck.summary || ''));
            if (ck.pairing || c.pairing) p('Pairing: ' + stPairing(ck.pairing || c.pairing) + ' · ' + (ck.volumes || 0) + ' volumes visible.');
            if (ck.warning) p(ck.warning);
            if (ck.lease) p(ck.lease);
            const row = document.createElement('div');
            row.className = 'connectivity-section__actions';
            if (!ck.found) {
                const create = document.createElement('button');
                create.className = 'btn btn--primary btn--sm';
                create.textContent = 'Create ' + (ck.folder || 'mokuro-reader') + '/';
                create.onclick = async () => {
                    try { await apiPost('/storage/connect/create', {}); showToast('Created.', 'success'); } catch (err) { showToast(err.message, 'error'); }
                    loadStorage();
                };
                row.appendChild(create);
            }
            if (ck.offer_switch && ck.other_pairing) {
                const sw = document.createElement('button');
                sw.className = 'btn btn--secondary btn--sm';
                sw.textContent = 'Try ' + stPairing(ck.other_pairing);
                sw.onclick = () => { stEl('sc-provider').value = 'gdrive'; storageConnectForm(); stEl('sc-reader-select').value = ck.other_pairing; storageConnect(null); };
                row.appendChild(sw);
            }
            live.appendChild(row);
            if (c.needs_restart && st.restart_pending) p('The server restarts to open it.');
            break;
        }
        case 'stopped': p(c.message || 'No code was used: click Connect for a new one.'); break;
        case 'failed': {
            p('Not connected: ' + (c.error || 'the sign-in failed'));
            if (c.suggest_flow) {
                // A personal Microsoft account with a code: the browser works.
                const again = document.createElement('button');
                again.className = 'btn btn--primary btn--sm';
                again.textContent = 'Sign in through the browser instead';
                again.onclick = () => { stEl('sc-provider').value = c.provider || 'onedrive'; storageConnectForm(); storageConnect(c.suggest_flow); };
                const row = document.createElement('div');
                row.className = 'connectivity-section__actions';
                row.appendChild(again);
                live.appendChild(row);
            }
            break;
        }
        case 'cancelled': live.hidden = true; break;
        default: live.hidden = true;
    }
}

// ---- Disconnect, pairing, lease -------------------------------------------------------

async function storageDisconnect(mode, waitS) {
    const st = storageState || {};
    let body = { mode: mode };
    if (mode === 'wait') {
        if (waitS == null && !confirm('Disconnect the ' + (st.provider || 'cloud') + ' library? Bunko uploads what it still owes first, then forgets its sign-in and local index. Nothing in the cloud is deleted.')) return false;
        body.wait_s = waitS || 20;
    } else if (mode === 'export') {
        // Bunko picks the folder (a new one under its storage folder's exports/); the
        // reply names it.
        if (!confirm('Copy the results not yet uploaded into a new folder under Bunko\'s storage folder (exports/), then disconnect?')) return false;
    } else if (mode === 'discard') {
        if (!confirm('Discard the results not yet uploaded and disconnect?')) return false;
    }
    stEl('st-owed').hidden = true;
    stEl('st-owed-actions').hidden = true;
    try {
        const r = await apiPost('/storage/disconnect', body);
        const rep = r.report || {};
        showToast('Disconnected' + (rep.exported && rep.exported.length ? ' (' + rep.exported.length + ' exported to ' + rep.export_dir + ')' : '') + (r.restarting ? '; the server restarts.' : '.'), 'success');
        if (r.restarting) setTimeout(() => window.location.reload(), 6000);
        loadStorage();
        return true;
    } catch (err) {
        if (err.status === 409 && err.payload && err.payload.owed) {
            stEl('st-owed').textContent = err.message;
            stEl('st-owed').hidden = false;
            stEl('st-owed-actions').hidden = false;
        } else {
            showToast(err.message, 'error');
        }
        return false;
    }
}

async function storagePairingChange() {
    const st = storageState || {};
    const other = st.pairing === 'beta' ? 'release' : 'beta';
    if (!confirm('Switch to the ' + stPairing(other) + ' pairing? That is a different library: Bunko disconnects this one (uploading what it owes), then signs in with the other reader\'s project.')) return;
    const done = await storageDisconnect('wait', 20);
    if (!done) return;
    stEl('sc-provider').value = 'gdrive';
    storageConnectForm();
    stEl('sc-reader-select').value = other;
    stEl('storage-connect').hidden = false;
    showToast('Disconnected. Connect with ' + stPairing(other) + ' once the server is back.', 'info');
}

async function storageKeepCopy(k, choice) {
    const keep = choice === 'this' ? (k.this || []) : (k.that || []);
    const gone = choice === 'this' ? (k.that || []) : (k.this || []);
    if (!confirm('Keep ' + keep.join(', ') + '? ' + gone.join(', ') + ' goes to the provider\'s trash (restorable there).')) return;
    try {
        await apiPost('/storage/conflicts/' + encodeURIComponent(k.id) + '/resolve', { choice: choice });
        showToast('The other copy goes to the trash; the merge finishes by itself.', 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
    loadStorage();
}

async function storageTakeOver() {
    if (!confirm('Take the library over from the other Bunko? Only do this when that one is gone for good: two Bunkos on one folder fight over it.')) return;
    try {
        await apiPost('/storage/lease/take-over', {});
        showToast('This Bunko manages the library now.', 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
    loadStorage();
}
