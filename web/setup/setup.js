// First-run setup: the admin account, who may join, remote access and OCR on this
// machine. The last step signs the new admin in and opens the admin panel, where the
// OCR backend install shows its progress.
(function () {
    'use strict';
    const $ = (id) => document.getElementById(id);
    let options = { ocr: null, pinned: {} };
    let steps = ['step-welcome', 'step-admin', 'step-registration', 'step-remote', 'step-ocr', 'step-done'];
    let current = 0;

    function show(index) {
        current = index;
        steps.forEach((id, i) => { $(id).classList.toggle('active', i === index); });
        $('setup-progress').innerHTML = steps.slice(0, -1).map((_, i) =>
            '<span class="setup-dot' + (i <= index ? ' active' : '') + '"></span>').join('');
        const first = $(steps[index]).querySelector('input:not([type=radio]):not([disabled]), select, button.btn--primary');
        if (first && index > 0) first.focus();
    }
    const next = () => show(Math.min(current + 1, steps.length - 1));
    const prev = () => show(Math.max(current - 1, 0));

    function showError(el, msg) {
        el.textContent = msg || '';
        el.hidden = !msg;
    }

    function checked(name) {
        const el = document.querySelector('input[name="' + name + '"]:checked');
        return el ? el.value : '';
    }

    // The GPUs found, as one line ('' for none).
    function gpus(hw) {
        const g = [];
        if (hw.nvidia_gpus && hw.nvidia_gpus.length) g.push('NVIDIA ' + hw.nvidia_gpus.join(', '));
        else if (hw.nvidia_driver) g.push('NVIDIA (driver ' + hw.nvidia_driver + ')');
        if (hw.amd_gfx && hw.amd_gfx.length) g.push('AMD ' + hw.amd_gfx.join(', '));
        return g.join('; ');
    }

    function setupOcr(o) {
        if (!o) {
            // This build reads no OCR itself: no step.
            steps = steps.filter((s) => s !== 'step-ocr');
            return;
        }
        const g = gpus(o.hardware);
        $('ocr-hw').textContent = g ? 'Found: ' + g + '.' : 'No GPU found: OCR runs on the CPU (slower).';
        $('ocr-on').checked = !!o.default_on;
        const sel = $('ocr-backend');
        sel.replaceChildren(...o.choices.map((c) => {
            const opt = document.createElement('option');
            opt.value = c.id;
            opt.textContent = c.id === 'auto' ? 'Auto (' + (o.hardware.auto_variant === 'cpu' ? 'CPU' : 'GPU') + ')' : c.label;
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

    async function finish() {
        const btn = $('finish-btn');
        btn.disabled = true;
        const username = $('admin-username').value.trim();
        const password = $('admin-password').value;
        const payload = {
            admin: { username, password },
            registration: { mode: checked('reg-mode') || 'self' },
            remote: remote(),
        };
        if (options.ocr) payload.ocr = { on: $('ocr-on').checked, backend: $('ocr-backend').value };
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
            if (/^(HTTPS|Dynamic DNS|remote|http)/i.test(msg) || /origin/.test(msg)) {
                show(steps.indexOf('step-remote'));
                showError($('remote-error'), msg);
            } else {
                show(steps.indexOf('step-admin'));
                showError($('admin-error'), msg);
            }
            return;
        }
        // Sign the new admin in (the password buys a bearer token for this tab, as
        // /_static/nav.js does; it is not kept).
        try {
            const t = await fetch('/login/api/token', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ username, password, kind: 'web', label: 'web page' }),
            });
            if (t.ok) {
                const td = await t.json();
                sessionStorage.setItem('mokuro_token', td.token);
                sessionStorage.setItem('mokuro_user', JSON.stringify(td.user));
            }
        } catch (_) { /* the login page then */ }
        // The admin panel shows these once (This server).
        const notes = (data.notes || []).slice();
        if (data.ocr && data.ocr.message && !data.ocr.installing) notes.push(data.ocr.message);
        try { sessionStorage.setItem('mokuro_setup_notes', JSON.stringify(notes)); } catch (_) { /* fine */ }
        $('done-notes').replaceChildren(...notes.map((n) => {
            const li = document.createElement('li');
            li.textContent = n;
            return li;
        }));
        show(steps.indexOf('step-done'));
        if (data.restarting) {
            const url = 'https://' + location.host + '/_admin#server';
            $('done-admin').href = url;
            $('done-text').textContent = 'Restarting with HTTPS…';
            await waitFor(url.replace('/_admin#server', '/api/health'), 30);
            $('done-text').textContent = 'Sign in at the HTTPS address.';
            return;
        }
        window.location.href = '/_admin#server';
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
        setupOcr(options.ocr);
        setupPinned(options.pinned || {});
        show(0);

        $('start-btn').addEventListener('click', next);
        document.querySelectorAll('[data-prev]').forEach((b) => b.addEventListener('click', prev));
        document.querySelectorAll('[data-next]').forEach((b) => b.addEventListener('click', next));
        $('admin-next').addEventListener('click', () => {
            const e = validateAdmin();
            showError($('admin-error'), e);
            if (!e) next();
        });
        $('remote-next').addEventListener('click', () => {
            const e = validateRemote();
            showError($('remote-error'), e);
            if (e) return;
            if (steps.includes('step-ocr')) next();
            else finish();
        });
        $('finish-btn').addEventListener('click', finish);
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
