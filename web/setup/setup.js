// First-run setup (one path, 0.8 §11): where the library lives (this computer, Google
// Drive, OneDrive, MEGA or a WebDAV server), Connect (one button: the server picks
// the sign-in flow), Check, whether readers connect to this Bunko, the admin account
// (then who may join and remote access, when readers connect), OCR on this machine
// (full build) or "Connect a processor" (lite build), Done. Done follows the first
// sync of a cloud library and the OCR of its first volume; a library on this disk
// opens the admin panel, where the OCR backend install shows its progress.
(function () {
    'use strict';
    const $ = (id) => document.getElementById(id);
    let options = { ocr: null, pinned: {}, library: null, local: false };
    let storage = null; // /setup/api/storage
    let where = 'local';
    let check = null; // the last Check
    let current = 'step-welcome';
    let polling = null;

    const NAMES = {
        local: 'this computer', gdrive: 'Google Drive', onedrive: 'OneDrive',
        mega: 'MEGA', webdav: 'the WebDAV server',
    };
    const OAUTH = ['gdrive', 'onedrive'];
    const isCloud = () => where !== 'local';
    const lite = () => !options.ocr;

    function checked(name) {
        const el = document.querySelector('input[name="' + name + '"]:checked');
        return el ? el.value : '';
    }

    function path() {
        const p = ['step-welcome', 'step-where'];
        if (isCloud()) p.push('step-connect', 'step-check');
        p.push('step-readers', 'step-admin');
        if (checked('readers') === 'yes') p.push('step-registration', 'step-remote');
        p.push(lite() ? 'step-processor' : 'step-ocr');
        p.push('step-done');
        return p;
    }

    function show(id) {
        current = id;
        const steps = path();
        document.querySelectorAll('.setup-step').forEach((el) => el.classList.toggle('active', el.id === id));
        const at = steps.indexOf(id);
        $('setup-progress').innerHTML = steps.slice(0, -1).map((_, i) =>
            '<span class="setup-dot' + (i <= at ? ' active' : '') + '"></span>').join('');
        const first = $(id).querySelector('input:not([type=radio]):not([type=checkbox]):not([disabled]), select, button.btn--primary');
        if (first && at > 0) first.focus();
        if (id === 'step-readers') enterReaders();
    }
    const next = () => { const p = path(); show(p[Math.min(p.indexOf(current) + 1, p.length - 1)]); };
    const prev = () => { const p = path(); show(p[Math.max(p.indexOf(current) - 1, 0)]); };

    function showError(el, msg) {
        el.textContent = msg || '';
        el.hidden = !msg;
    }

    async function getJSON(url, token) {
        const headers = token ? { Authorization: 'Bearer ' + token } : {};
        const r = await fetch(url, { headers, cache: 'no-store' });
        const data = await r.json().catch(() => ({}));
        if (!r.ok) throw Object.assign(new Error(data.error || ('HTTP ' + r.status)), { status: r.status, data });
        return data;
    }

    async function postJSON(url, body) {
        const r = await fetch(url, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body || {}),
        });
        const data = await r.json().catch(() => ({}));
        if (!r.ok) throw Object.assign(new Error(data.error || ('HTTP ' + r.status)), { status: r.status, data });
        return data;
    }

    // ---- where does the library live ---------------------------------------------

    function setupWhere() {
        const lib = options.library || {};
        if (lib.local_folder) $('where-local-folder').textContent = 'The library folder on this machine: ' + lib.local_folder;
        const configured = (storage && storage.configured && storage.configured.backend) || lib.backend || 'local';
        const radio = document.querySelector('input[name="where"][value="' + configured + '"]');
        if (radio) radio.checked = true;
        const pinned = (options.pinned || {}).backend;
        if (pinned) {
            $('where-pinned').textContent = 'Set by MOKURO_LIBRARY_BACKEND: ' + pinned + '.';
            $('where-pinned').hidden = false;
            document.querySelectorAll('input[name="where"]').forEach((r) => {
                r.checked = r.value === pinned;
                r.disabled = r.value !== pinned;
            });
        }
        if (!storage) {
            // This server cannot change where its library lives (an older one).
            document.querySelectorAll('input[name="where"]').forEach((r) => { r.disabled = r.value !== 'local'; });
        }
    }

    async function whereNext() {
        where = checked('where') || 'local';
        showError($('where-error'), '');
        if (where === 'local') {
            if (storage && storage.configured && storage.configured.backend !== 'local') {
                try {
                    await postJSON('/setup/api/storage/local', {});
                } catch (e) {
                    showError($('where-error'), e.message);
                    return;
                }
            }
            next();
            return;
        }
        setupConnect();
        next();
    }

    // ---- Connect ---------------------------------------------------------------------

    function setupConnect() {
        const name = NAMES[where];
        const conf = (storage && storage.configured) || {};
        $('connect-title').textContent = 'Connect ' + name;
        $('connect-lede').textContent = OAUTH.includes(where)
            ? (options.local
                ? 'One sign-in: the ' + (where === 'gdrive' ? 'Google' : 'Microsoft') + ' page opens in a new tab.'
                : 'One sign-in: you get a code to enter on any device, a phone included.')
            : where === 'mega' ? 'Your MEGA account, once.' : 'The server\'s address, your user and an app password.';
        const custom = where === 'gdrive' && conf.custom_pairing;
        $('connect-reader').hidden = where !== 'gdrive' || custom;
        $('connect-custom').hidden = !custom;
        if (custom) $('connect-custom').textContent = 'Pairing: ' + (conf.custom_pairing_label || 'custom') + ' (storage.library.oauth.google): Bunko signs in with those clients.';
        if (where === 'gdrive' && conf.google_project && conf.google_project !== 'custom') {
            const r = document.querySelector('input[name="reader"][value="' + conf.google_project + '"]');
            if (r) r.checked = true;
        }
        if ((options.pinned || {}).google_project || (conf.pinned || {}).google_project) {
            document.querySelectorAll('input[name="reader"]').forEach((r) => { r.disabled = true; });
        }
        $('connect-mega').hidden = where !== 'mega';
        $('connect-webdav').hidden = where !== 'webdav';
        if (where === 'webdav' && conf.webdav) {
            if (conf.webdav.url && !$('dav-url').value) $('dav-url').value = conf.webdav.url;
            if (conf.webdav.username && !$('dav-user').value) $('dav-user').value = conf.webdav.username;
            $('dav-http').checked = !!conf.webdav.allow_http;
        }
        $('connect-code-link').hidden = !(OAUTH.includes(where) && options.local);
        $('connect-btn').textContent = 'Connect';
        $('connect-live').hidden = true;
        showError($('connect-error'), '');
    }

    function connectBody(forceDevice) {
        const body = { provider: where };
        if (where === 'gdrive' && $('connect-reader').hidden === false) body.reader = checked('reader') || 'release';
        if (where === 'mega') {
            body.mega = { email: $('mega-email').value.trim(), password: $('mega-password').value, totp: $('mega-totp').value.trim() || null };
        }
        if (where === 'webdav') {
            body.webdav = {
                url: $('dav-url').value.trim(), username: $('dav-user').value.trim(),
                password: $('dav-password').value, allow_http: $('dav-http').checked,
            };
        }
        if (forceDevice) body.flow = 'device';
        return body;
    }

    function validateConnect() {
        if (where === 'mega' && (!$('mega-email').value.trim() || !$('mega-password').value)) return 'Enter the MEGA email and password';
        if (where === 'webdav') {
            const url = $('dav-url').value.trim();
            if (!url || !$('dav-user').value.trim() || !$('dav-password').value) return 'Enter the server address, the user and the app password';
            if (/^http:\/\//i.test(url) && !$('dav-http').checked) return 'http:// sends the password unencrypted: use https://, or allow http for a server on your own network';
        }
        return null;
    }

    async function connect(forceDevice) {
        const err = validateConnect();
        showError($('connect-error'), err);
        if (err) return;
        stopPolling();
        // The loopback sign-in's tab, opened while this click still counts (pop-up
        // blockers); it gets the sign-in page once the server says which flow runs.
        let tab = null;
        if (OAUTH.includes(where) && options.local && !forceDevice) {
            tab = window.open('', '_blank');
        }
        $('connect-btn').disabled = true;
        try {
            await postJSON('/setup/api/storage/connect', connectBody(forceDevice));
        } catch (e) {
            if (tab) tab.close();
            $('connect-btn').disabled = false;
            showError($('connect-error'), e.message);
            return;
        }
        // Sent once: the passwords are not kept in the page.
        $('mega-password').value = '';
        $('mega-totp').value = '';
        pollConnect(tab);
    }

    function stopPolling() {
        if (polling) clearTimeout(polling);
        polling = null;
    }

    function pollConnect(tab) {
        let opened = false;
        const tick = async () => {
            let s;
            try {
                s = await getJSON('/setup/api/storage/connect');
            } catch (e) {
                showError($('connect-error'), e.message);
                $('connect-btn').disabled = false;
                return;
            }
            if (s.state === 'browser' && tab && !opened && s.auth_url) {
                try { tab.location.href = s.auth_url; opened = true; } catch (_) { /* the link below */ }
            }
            if (s.state !== 'browser' && tab && !opened) { tab.close(); tab = null; }
            renderConnect(s);
            if (s.state === 'connected') {
                check = s.check;
                stopPolling();
                $('connect-btn').disabled = false;
                renderCheck(s);
                show('step-check');
                return;
            }
            if (['failed', 'stopped', 'cancelled', 'account-mismatch', 'idle'].includes(s.state)) {
                $('connect-btn').disabled = false;
                if (s.state !== 'account-mismatch') return;
                return;
            }
            polling = setTimeout(tick, 1000);
        };
        tick();
    }

    function countdown(iso) {
        if (!iso) return '';
        const left = Math.max(0, Math.round((new Date(iso).getTime() - Date.now()) / 1000));
        return Math.floor(left / 60) + ':' + String(left % 60).padStart(2, '0');
    }

    function renderConnect(s) {
        const live = $('connect-live');
        const name = NAMES[s.provider] || NAMES[where];
        live.hidden = false;
        live.replaceChildren();
        const p = (text) => { const el = document.createElement('p'); el.textContent = text; live.appendChild(el); return el; };
        showError($('connect-error'), '');
        switch (s.state) {
            case 'starting': p('Starting the sign-in…'); break;
            case 'browser': {
                p('Sign in to ' + name + ' in the tab that opened, then come back here.');
                const a = document.createElement('a');
                a.href = s.auth_url; a.target = '_blank'; a.rel = 'noopener';
                a.textContent = 'Open the sign-in page';
                live.appendChild(a);
                p('Waiting ' + countdown(s.expires_at) + ' more.');
                break;
            }
            case 'code': {
                const intro = p('Open ');
                const a = document.createElement('a');
                a.href = s.verification_url; a.target = '_blank'; a.rel = 'noopener';
                a.textContent = s.verification_url;
                intro.appendChild(a);
                intro.appendChild(document.createTextNode(' on any device and enter:'));
                const code = document.createElement('span');
                code.className = 'connect-code';
                code.textContent = s.user_code;
                live.appendChild(code);
                p('Code ' + s.code_number + ' of ' + s.max_codes + ' · expires in ' + countdown(s.expires_at) + '; a new one follows by itself.');
                break;
            }
            case 'checking': p(s.message || ('Checking ' + name + '…')); break;
            case 'account-mismatch': {
                p(s.message || 'This is another account.');
                const row = document.createElement('div');
                row.className = 'setup-actions';
                const use = document.createElement('button');
                use.className = 'btn btn--danger btn--sm';
                use.textContent = 'Use this account\'s library instead';
                use.onclick = async () => {
                    try {
                        await postJSON('/setup/api/storage/connect/account', { use_this_account: true });
                        pollConnect(null);
                    } catch (e) { showError($('connect-error'), e.message); }
                };
                const no = document.createElement('button');
                no.className = 'btn btn--secondary btn--sm';
                no.textContent = 'Cancel';
                no.onclick = async () => {
                    await postJSON('/setup/api/storage/connect/account', { use_this_account: false }).catch(() => {});
                    live.hidden = true;
                };
                row.append(use, no);
                live.appendChild(row);
                break;
            }
            case 'stopped':
                p(s.message || 'No code was used.');
                $('connect-btn').textContent = 'Get a new code';
                break;
            case 'failed':
                live.hidden = true;
                showError($('connect-error'), s.error || 'The sign-in failed.');
                break;
            case 'cancelled':
                live.hidden = true;
                break;
            case 'connected':
                p('Connected as ' + (s.account || 'your account') + '.');
                break;
            default:
                live.hidden = true;
        }
    }

    // ---- Check -------------------------------------------------------------------------

    function pairingLabel(p) {
        if (p === 'release') return 'reader.mokuro.app (release)';
        if (p === 'beta') return 'Mokuro Reader beta';
        if (p && p.startsWith('custom:')) return 'custom (' + p.slice(7) + ')';
        return p || '';
    }

    function renderCheck(s) {
        const c = s.check || {};
        $('check-account').textContent = 'Signed in to ' + (NAMES[s.provider] || '') + ' as ' + (s.account || '?') + '.';
        $('check-summary').textContent = c.summary || '';
        const pairing = c.pairing || s.pairing;
        $('check-pairing').hidden = !pairing;
        if (pairing) $('check-pairing').textContent = 'Pairing: ' + pairingLabel(pairing) + ' · ' + (c.volumes || 0) + ' volume' + (c.volumes === 1 ? '' : 's') + ' visible.';
        $('check-warning').hidden = !c.warning;
        $('check-warning').textContent = c.warning || '';
        $('check-lease').hidden = !c.lease;
        $('check-lease').textContent = c.lease || '';
        $('check-switch').hidden = !(c.offer_switch && c.other_pairing);
        if (c.other_pairing) $('check-switch').textContent = 'Try ' + pairingLabel(c.other_pairing);
        $('check-create').hidden = !!c.found;
        $('check-create').textContent = 'Create ' + (c.folder || 'mokuro-reader') + '/';
        $('check-offers').hidden = $('check-switch').hidden && $('check-create').hidden;
        $('check-next').disabled = !c.found;
        showError($('check-error'), '');
    }

    async function createFolder() {
        $('check-create').disabled = true;
        try {
            const r = await postJSON('/setup/api/storage/connect/create', {});
            const s = await getJSON('/setup/api/storage/connect');
            s.check = r.check;
            renderCheck(s);
        } catch (e) {
            showError($('check-error'), e.message);
        } finally {
            $('check-create').disabled = false;
        }
    }

    function switchPairing() {
        const other = check && check.other_pairing;
        const r = other && document.querySelector('input[name="reader"][value="' + other + '"]');
        if (r) r.checked = true;
        show('step-connect');
        connect(false);
    }

    // ---- readers -----------------------------------------------------------------------

    let readersSet = false;
    function enterReaders() {
        const name = NAMES[where];
        $('readers-no-text').textContent = isCloud()
            ? 'Mokuro Reader keeps reading from your ' + (where === 'webdav' ? 'WebDAV server' : name) + '; Bunko adds OCR, covers and series info.'
            : 'Bunko only OCRs the library and adds covers and series info here.';
        if (!readersSet) {
            readersSet = true;
            const def = isCloud() ? 'no' : 'yes';
            const r = document.querySelector('input[name="readers"][value="' + def + '"]');
            if (r) r.checked = true;
        }
        const pinned = (options.pinned || {}).serve_readers;
        if (pinned != null) {
            $('readers-pinned').textContent = 'Set by MOKURO_SERVE_READERS (' + (pinned ? 'yes' : 'no') + ').';
            $('readers-pinned').hidden = false;
            document.querySelectorAll('input[name="readers"]').forEach((r) => {
                r.checked = (r.value === 'yes') === !!pinned;
                r.disabled = true;
            });
        }
        readersLink();
    }

    function readersLink() {
        $('readers-link-row').hidden = !(isCloud() && checked('readers') === 'yes');
    }

    // ---- OCR (full build) ------------------------------------------------------------

    function gpus(hw) {
        const g = [];
        if (hw.nvidia_gpus && hw.nvidia_gpus.length) g.push('NVIDIA ' + hw.nvidia_gpus.join(', '));
        else if (hw.nvidia_driver) g.push('NVIDIA (driver ' + hw.nvidia_driver + ')');
        if (hw.amd_gfx && hw.amd_gfx.length) g.push('AMD ' + hw.amd_gfx.join(', '));
        if (hw.vulkan_gpus && hw.vulkan_gpus.length) g.push(hw.vulkan_gpus.join(', '));
        return g.join('; ');
    }

    function setupOcr(o) {
        if (!o) return;
        const g = gpus(o.hardware);
        $('ocr-hw').textContent = g ? 'Found: ' + g + '.' : 'No GPU found: OCR runs on the CPU (slower).';
        $('ocr-on').checked = !!o.default_on;
        const sel = $('ocr-backend');
        sel.replaceChildren(...o.choices.map((c) => {
            const opt = document.createElement('option');
            opt.value = c.id;
            opt.textContent = c.id === 'auto' ? 'Auto (' + (o.hardware.auto_variant === 'cpu' && !o.hardware.vulkan_pack ? 'CPU' : 'GPU') + ')' : c.label;
            return opt;
        }));
        sel.value = o.backend_locked ? o.backend : 'auto';
        if (o.backend_locked) {
            sel.disabled = true;
            $('ocr-backend-note').textContent = o.backend_locked + '.';
            $('ocr-backend-note').hidden = false;
        }
        if (o.local_locked) {
            $('ocr-on').checked = !/^(0|false|no|off)$/i.test(o.local_locked);
            $('ocr-on').disabled = true;
        }
        const note = () => {
            const on = $('ocr-on').checked;
            $('ocr-backend-group').hidden = !on;
            $('ocr-note').textContent = !on ? 'You can turn it on later in the admin panel (This server).'
                : o.auto_install ? 'The OCR backend downloads in the background after setup (several GB for a GPU).'
                    : 'Automatic installs are off here: install the backend in the admin panel (This server).';
        };
        $('ocr-on').addEventListener('change', note);
        note();
    }

    function setupPinned(p) {
        if (p.registration) {
            $('reg-pinned').textContent = 'Set by MOKURO_REGISTRATION_MODE: ' + p.registration + '.';
            $('reg-pinned').hidden = false;
            document.querySelectorAll('input[name="reg-mode"]').forEach((r) => {
                r.checked = r.value === p.registration;
                r.disabled = true;
            });
        }
        if (p.ssl != null) {
            $('ssl-group').hidden = true;
            $('ssl-pinned').textContent = 'HTTPS is set by MOKURO_SSL_ENABLED (' + (p.ssl ? 'on' : 'off') + ').';
            $('ssl-pinned').hidden = false;
        }
    }

    // ---- admin, remote access ------------------------------------------------------------

    function validateAdmin() {
        const username = $('admin-username').value.trim();
        const password = $('admin-password').value;
        if (!/^[a-zA-Z0-9_-]{3,32}$/.test(username)) return 'Username: 3–32 letters, digits, _ or -';
        if (password.length < 8) return 'Password: at least 8 characters';
        if (password !== $('admin-password-confirm').value) return 'The passwords differ';
        return null;
    }

    function remote() {
        const access = checked('access');
        const r = { access };
        if (access === 'dyndns') {
            r.dyndns = {
                provider: $('dd-provider').value,
                domain: $('dd-domain').value.trim(),
                token: $('dd-token').value.trim(),
                update_url: $('dd-url').value.trim(),
            };
        }
        if (!$('ssl-group').hidden) {
            r.ssl = { mode: $('ssl-mode').value, cert_file: $('cert-file').value.trim(), key_file: $('key-file').value.trim() };
        }
        r.cors_origins = $('cors').value.split('\n').map((s) => s.trim()).filter(Boolean);
        return r;
    }

    function validateRemote() {
        const r = remote();
        if (r.access === 'dyndns') {
            if (!r.dyndns.domain || !r.dyndns.token) return 'Dynamic DNS needs the domain and the token';
            if (r.dyndns.provider === 'generic' && !r.dyndns.update_url) return 'Enter the update URL';
        }
        if (r.ssl && r.ssl.mode === 'files' && (!r.ssl.cert_file || !r.ssl.key_file)) return 'Enter both certificate files';
        return null;
    }

    // ---- finish and Done -------------------------------------------------------------------

    async function finish(btn) {
        btn.disabled = true;
        const username = $('admin-username').value.trim();
        const password = $('admin-password').value;
        const readers = checked('readers') === 'yes';
        const payload = {
            admin: { username, password },
            registration: { mode: readers ? (checked('reg-mode') || 'self') : 'disabled' },
            library: { serve_readers: readers, link_progress: readers && isCloud() && $('readers-link').checked },
        };
        if (readers) payload.remote = remote();
        if (options.ocr) payload.ocr = { on: $('ocr-on').checked, backend: $('ocr-backend').value };
        if (lite()) payload.processor = { create: $('proc-create').checked };
        let data = {};
        let ok = false;
        try {
            const resp = await fetch('/setup/api/complete', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(payload),
            });
            data = await resp.json().catch(() => ({}));
            ok = resp.ok;
        } catch (err) {
            data = { error: 'Connection error: ' + err.message };
        }
        btn.disabled = false;
        if (!ok) {
            const msg = data.error || 'Setup failed';
            if (readers && (/^(HTTPS|Dynamic DNS|remote|http)/i.test(msg) || /origin/.test(msg))) {
                show('step-remote');
                showError($('remote-error'), msg);
            } else {
                show('step-admin');
                showError($('admin-error'), msg);
            }
            return;
        }
        // Sign the new admin in (the password buys a bearer token for this tab, as
        // /_static/nav.js does; it is not kept).
        let token = null;
        try {
            const t = await fetch('/login/api/token', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ username, password, kind: 'web', label: 'web page' }),
            });
            if (t.ok) {
                const td = await t.json();
                token = td.token;
                sessionStorage.setItem('mokuro_token', td.token);
                sessionStorage.setItem('mokuro_user', JSON.stringify(td.user));
            }
        } catch (_) { /* the login page then */ }
        const notes = (data.notes || []).slice();
        if (data.ocr && data.ocr.message && !data.ocr.installing) notes.push(data.ocr.message);
        try { sessionStorage.setItem('mokuro_setup_notes', JSON.stringify(notes)); } catch (_) { /* fine */ }
        $('done-notes').replaceChildren(...notes.map((n) => {
            const li = document.createElement('li');
            li.textContent = n;
            return li;
        }));
        show('step-done');
        const pairing = data.processor && data.processor.username ? data.processor : null;
        if (pairing) showPairing(pairing, token);
        if (data.restarting && data.https) {
            const url = 'https://' + location.host + '/_admin#server';
            $('done-admin').href = url;
            $('done-text').textContent = 'Restarting with HTTPS…';
            await waitFor(url.replace('/_admin#server', '/api/health'), 30);
            $('done-text').textContent = 'Sign in at the HTTPS address.';
            return;
        }
        if (isCloud()) {
            $('done-admin').href = '/_admin#storage';
            if (data.restarting) {
                $('done-text').textContent = 'Opening the library: the server restarts…';
                await waitFor('/api/health', 60);
            }
            followFirstSync(token);
            return;
        }
        if (pairing) {
            $('done-text').textContent = 'Pair a processor to start the OCR.';
            return;
        }
        window.location.href = '/_admin#server';
    }

    function showPairing(p, token) {
        const url = location.origin;
        $('done-pairing').hidden = false;
        $('done-pair-url').textContent = url;
        $('done-pair-user').textContent = p.username;
        $('done-pair-password').textContent = p.password;
        $('done-pair-command').textContent = 'mokuro-bunko processor setup --url ' + url + ' --username ' + p.username + ' --password-stdin';
        if (!token) return;
        const tick = async () => {
            try {
                const r = await getJSON('/_admin/api/processors', token);
                const list = (r.processors || []).filter((x) => x && (x.connected !== false));
                if (list.length) {
                    const n = list[0];
                    $('done-pair-wait').textContent = 'Processor ' + (n.public_name || n.name || n.username || '') + ' connected.';
                    return;
                }
            } catch (_) { /* the server may be restarting */ }
            setTimeout(tick, 3000);
        };
        tick();
    }

    // Done for a cloud library: the first sync, then the OCR of the first volume owed.
    function followFirstSync(token) {
        $('done-progress').hidden = false;
        const name = NAMES[where];
        if (!token) {
            $('done-text').textContent = 'Set up. Sign in to the admin panel to follow the first sync.';
            return;
        }
        $('done-text').textContent = 'Set up. Bunko now reads the library in ' + name + '.';
        let synced = false;
        let firstJob = null;
        let uploading = null; // the first OCR result seen waiting to go up
        let idle = 0; // polls with nothing to OCR and nothing owed
        const tick = async () => {
            let st = null;
            try { st = await getJSON('/_admin/api/storage', token); } catch (_) { /* restarting */ }
            if (st && !synced) {
                const h = st.health || {};
                if (h.state === 'syncing') {
                    const d = (st.sync && st.sync.done) || h.done || 0;
                    const t = (st.sync && st.sync.total) || h.total || 0;
                    $('done-sync').textContent = t ? 'First sync: ' + d + ' of ' + t : 'First sync: ' + d + ' files listed so far…';
                    $('done-sync-bar').hidden = !t;
                    if (t) $('done-sync-fill').style.width = Math.min(100, (100 * d) / t).toFixed(1) + '%';
                } else if (h.state === 'online') {
                    synced = true;
                    $('done-sync-bar').hidden = true;
                    $('done-sync').textContent = 'In sync with ' + name + '.';
                } else if ((st.problems || []).length) {
                    const p = st.problems[0];
                    $('done-sync').textContent = p.text + (p.hint ? ' ' + p.hint : '');
                } else {
                    $('done-sync').textContent = 'Connecting to ' + name + '…';
                }
            }
            if (synced) {
                let q = null;
                try { q = await getJSON('/queue/api/status', token); } catch (_) { /* OCR starting */ }
                // An OCR result the store still owes the cloud (the setup is accepted
                // once it has gone up).
                const owed = ((st && st.outbox && st.outbox.owed_ops) || []).filter((o) => /^ocr/.test(o.origin || '') && /\.mokuro$/.test(o.path || ''));
                if (owed.length && !uploading) uploading = owed[0].path;
                if (q) {
                    const jobs = [];
                    (q.machines || []).forEach((m) => (m.jobs || []).forEach((j) => jobs.push(j)));
                    const label = (j) => (j.series ? j.series + ' · ' : '') + (j.volume || '');
                    const pending = typeof q.pending_count === 'number' ? q.pending_count : (q.pending || []).length;
                    const claiming = st && st.lease && ['claiming', 'not-started'].includes(st.lease.state);
                    idle = jobs.length || pending || owed.length ? 0 : idle + 1;
                    if (jobs.length) {
                        const j = jobs[0];
                        if (!firstJob) firstJob = label(j);
                        $('done-ocr').textContent = 'OCR: ' + label(j) + ' — ' + Math.round(j.percent || 0) + '%';
                    } else if (uploading && owed.length) {
                        $('done-ocr').textContent = 'OCR\'d ' + uploading.replace(/\.mokuro$/, '') + '. ' + (claiming
                            ? 'Its .mokuro goes up to ' + name + ' once this Bunko holds the library\'s lease (the first claim waits a few minutes, so that two Bunkos never share a folder).'
                            : 'Uploading its .mokuro to ' + name + '…');
                    } else if (uploading) {
                        $('done-ocr').textContent = 'OCR\'d ' + uploading.replace(/\.mokuro$/, '') + ': its .mokuro is in ' + name + '. ' + (pending ? pending + ' more to go.' : 'Nothing else is owed.');
                        return;
                    } else if (pending) {
                        $('done-ocr').textContent = (q.machines || []).length
                            ? pending + ' volume' + (pending === 1 ? '' : 's') + ' wait for OCR…'
                            : pending + ' volume' + (pending === 1 ? '' : 's') + ' wait for OCR: ' + (lite() ? 'pair a processor.' : 'the OCR backend is getting ready (This server shows its install).');
                    } else if (idle >= 8) {
                        $('done-ocr').textContent = 'Every volume already has its OCR.';
                        return;
                    } else {
                        $('done-ocr').textContent = 'Looking for volumes to OCR…';
                    }
                }
            }
            setTimeout(tick, 2500);
        };
        tick();
    }

    // Wait until `url` answers (a restart), up to `secs` seconds.
    async function waitFor(url, secs) {
        const end = Date.now() + secs * 1000;
        await new Promise((r) => setTimeout(r, 1500));
        while (Date.now() < end) {
            try {
                await fetch(url, { mode: 'no-cors', cache: 'no-store' });
                return true;
            } catch (_) { /* not yet */ }
            await new Promise((r) => setTimeout(r, 1000));
        }
        return false;
    }

    document.addEventListener('DOMContentLoaded', async () => {
        try {
            const resp = await fetch('/setup/api/status');
            const data = await resp.json();
            if (!data.needs_setup) {
                window.location.href = '/';
                return;
            }
        } catch (_) { /* go on */ }
        try {
            const r = await fetch('/setup/api/options');
            if (r.ok) options = await r.json();
        } catch (_) { /* the defaults */ }
        try {
            storage = await getJSON('/setup/api/storage');
        } catch (_) { storage = null; }
        if (!options.local) {
            // Files on the server can be named on the server itself only.
            const files = document.querySelector('#ssl-mode option[value="files"]');
            if (files) files.remove();
        }
        setupOcr(options.ocr);
        setupPinned(options.pinned || {});
        setupWhere();
        show('step-welcome');

        $('start-btn').addEventListener('click', next);
        document.querySelectorAll('[data-prev]').forEach((b) => b.addEventListener('click', prev));
        document.querySelectorAll('[data-next]').forEach((b) => b.addEventListener('click', next));
        $('where-next').addEventListener('click', whereNext);
        $('connect-btn').addEventListener('click', () => connect(false));
        $('connect-back').addEventListener('click', () => { stopPolling(); show('step-where'); });
        $('connect-code-link').addEventListener('click', (ev) => { ev.preventDefault(); connect(true); });
        $('check-back').addEventListener('click', () => { setupConnect(); show('step-connect'); });
        $('check-next').addEventListener('click', next);
        $('check-create').addEventListener('click', createFolder);
        $('check-switch').addEventListener('click', switchPairing);
        document.querySelectorAll('input[name="readers"]').forEach((r) => r.addEventListener('change', readersLink));
        $('readers-next').addEventListener('click', next);
        $('admin-next').addEventListener('click', () => {
            const e = validateAdmin();
            showError($('admin-error'), e);
            if (!e) next();
        });
        $('remote-next').addEventListener('click', () => {
            const e = validateRemote();
            showError($('remote-error'), e);
            if (!e) next();
        });
        $('finish-btn').addEventListener('click', () => finish($('finish-btn')));
        $('proc-finish').addEventListener('click', () => finish($('proc-finish')));
        document.querySelectorAll('input[name="access"]').forEach((r) => r.addEventListener('change', () => {
            $('dyndns-fields').hidden = checked('access') !== 'dyndns';
        }));
        $('dd-provider').addEventListener('change', () => { $('dd-url-group').hidden = $('dd-provider').value !== 'generic'; });
        $('ssl-mode').addEventListener('change', () => {
            $('ssl-files').hidden = $('ssl-mode').value !== 'files';
            $('ssl-note').hidden = $('ssl-mode').value === 'off';
        });
    });
})();
