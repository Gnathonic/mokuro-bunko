// Admin Panel JavaScript

const API_BASE = '/_admin/api';

// State
let users = [];
let invites = [];
let auditEvents = [];
let statusRefreshTimer = null;
let corsOrigins = [];

// DOM Elements
const usersBody = document.getElementById('users-body');
const invitesBody = document.getElementById('invites-body');
const auditBody = document.getElementById('audit-body');

// Auth helper - the signed-in tab's bearer token (see /_static/nav.js)
function getAuthHeaders() {
    return window.mokuroAuth.headers();
}

async function logout() {
    await window.mokuroAuth.signOut();
    window.location.href = '/';
}

function getSessionRole() {
    try {
        const user = JSON.parse(sessionStorage.getItem('mokuro_user'));
        return user && user.role ? user.role : null;
    } catch (_) {
        return null;
    }
}

// Inviters only get the invite API; reduce the panel to the Invites tab
function trimToInvitesOnly() {
    document.querySelectorAll('.tab').forEach(tab => {
        if (tab.dataset.tab !== 'invites') tab.remove();
    });
    document.querySelectorAll('.tab-content').forEach(c => c.classList.remove('active'));
    document.querySelector(".tab[data-tab='invites']").classList.add('active');
    document.getElementById('invites-tab').classList.add('active');
}

// Initialize
document.addEventListener('DOMContentLoaded', () => {
    if (!window.mokuroAuth.token()) {
        window.location.href = '/login';
        return;
    }
    const isInviter = getSessionRole() === 'inviter';
    if (isInviter) {
        trimToInvitesOnly();
    }
    initTabs();
    initModals();
    initForms();
    if (!isInviter) {
        loadUsers();
    }
    loadInvites();
    // A refresh on the Audit tab comes back to it, filters and all.
    if (!isInviter && (window.location.hash || '').startsWith('#audit')) {
        const tab = document.querySelector(".tab[data-tab='audit']");
        if (tab) tab.click();
    }
});

// Tab switching
function initTabs() {
    document.querySelectorAll('.tab').forEach(tab => {
        tab.addEventListener('click', () => {
            const tabId = tab.dataset.tab;

            // Update tab buttons
            document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
            tab.classList.add('active');

            // Update tab content
            document.querySelectorAll('.tab-content').forEach(c => c.classList.remove('active'));
            document.getElementById(`${tabId}-tab`).classList.add('active');

            // Lazy-load tab data
            if (tabId === 'settings') {
                loadSettings();
                initSettingsRail();
                startProcessorPolling();
            } else {
                stopProcessorPolling();
            }
            if (tabId === 'audit') {
                loadAudit();
            } else if ((window.location.hash || '').startsWith('#audit')) {
                try { history.replaceState(null, '', window.location.pathname + window.location.search); } catch (_) { /* ignore */ }
            }
            if (tabId === 'status') {
                loadStatus();
                loadUpdate(false);
                startStatusRefresh();
            } else {
                stopStatusRefresh();
            }
            if (tabId === 'connectivity') {
                loadTunnelStatus();
                loadDynDNSStatus();
            }
        });
    });
}

// Settings rail: highlight the section in view, smooth-scroll on click
let settingsRailReady = false;
function initSettingsRail() {
    if (settingsRailReady) return;
    settingsRailReady = true;
    const links = Array.from(document.querySelectorAll('.settings-rail a'));
    const sections = links
        .map(a => document.querySelector(a.getAttribute('href')))
        .filter(Boolean);
    const setCurrent = (id) => links.forEach(a => a.classList.toggle('is-current', a.getAttribute('href') === '#' + id));
    links.forEach(a => a.addEventListener('click', (e) => {
        const target = document.querySelector(a.getAttribute('href'));
        if (!target) return;
        e.preventDefault();
        target.scrollIntoView({ behavior: 'smooth', block: 'start' });
        setCurrent(target.id);
    }));
    if ('IntersectionObserver' in window) {
        const observer = new IntersectionObserver((entries) => {
            const visible = entries.filter(en => en.isIntersecting).sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top);
            if (visible.length) setCurrent(visible[0].target.id);
        }, { rootMargin: '-20% 0px -60% 0px' });
        sections.forEach(s => observer.observe(s));
    }
    if (sections.length) setCurrent(sections[0].id);
}

// Modal handling
function initModals() {
    // Close modal buttons
    document.querySelectorAll('[data-close-modal]').forEach(btn => {
        btn.addEventListener('click', () => {
            btn.closest('.modal-overlay').classList.remove('open');
        });
    });

    // Close modal on backdrop click
    document.querySelectorAll('.modal-overlay').forEach(modal => {
        modal.addEventListener('click', (e) => {
            if (e.target === modal) {
                modal.classList.remove('open');
            }
        });
    });

    // Open modal buttons
    document.getElementById('add-user-btn').addEventListener('click', () => {
        document.getElementById('add-user-form').reset();
        openModal('add-user-modal');
    });

    document.getElementById('generate-invite-btn').addEventListener('click', () => {
        document.getElementById('generate-invite-form').reset();
        openModal('generate-invite-modal');
    });

    // Copy invite code button
    document.getElementById('copy-invite-btn').addEventListener('click', () => {
        const input = document.getElementById('invite-code-value');
        input.select();
        document.execCommand('copy');
        showToast('Copied to clipboard', 'success');
    });
}

function openModal(id) {
    document.getElementById(id).classList.add('open');
}

function closeModal(id) {
    document.getElementById(id).classList.remove('open');
}

// Form handling
function initForms() {
    // Add user form
    document.getElementById('add-user-form').addEventListener('submit', async (e) => {
        e.preventDefault();
        const form = e.target;
        const data = {
            username: form.username.value,
            password: form.password.value,
            role: form.role.value,
        };

        try {
            await apiPost('/users', data);
            closeModal('add-user-modal');
            showToast('User created', 'success');
            loadUsers();
        } catch (err) {
            showToast(err.message, 'error');
        }
    });

    // Generate invite form
    document.getElementById('generate-invite-form').addEventListener('submit', async (e) => {
        e.preventDefault();
        const form = e.target;
        const data = {
            role: form.role.value,
            expires: form.expires.value,
        };

        try {
            const result = await apiPost('/invites', data);
            closeModal('generate-invite-modal');

            // Show the generated code
            document.getElementById('invite-code-value').value = result.invite.code;
            openModal('invite-code-modal');

            loadInvites();
        } catch (err) {
            showToast(err.message, 'error');
        }
    });

    // Change role form
    document.getElementById('change-role-form').addEventListener('submit', async (e) => {
        e.preventDefault();
        const form = e.target;
        const username = form.username.value;
        const role = form.role.value;

        try {
            await apiPut(`/users/${encodeURIComponent(username)}/role`, { role });
            closeModal('change-role-modal');
            showToast('Role updated', 'success');
            loadUsers();
        } catch (err) {
            showToast(err.message, 'error');
        }
    });

    // Edit notes form
    document.getElementById('edit-notes-form').addEventListener('submit', async (e) => {
        e.preventDefault();
        const form = e.target;
        const username = form.username.value;
        const notes = form.notes.value;

        try {
            await apiPut(`/users/${encodeURIComponent(username)}/notes`, { notes });
            closeModal('edit-notes-modal');
            showToast('Notes updated', 'success');
            loadUsers();
        } catch (err) {
            showToast(err.message, 'error');
        }
    });

    // Confirm delete button
    document.getElementById('confirm-delete-btn').addEventListener('click', async () => {
        const modal = document.getElementById('confirm-delete-modal');
        const type = modal.dataset.deleteType;
        const id = modal.dataset.deleteId;

        try {
            if (type === 'user') {
                await apiDelete(`/users/${encodeURIComponent(id)}`);
                showToast('User deleted', 'success');
                loadUsers();
            } else if (type === 'invite') {
                await apiDelete(`/invites/${encodeURIComponent(id)}`);
                showToast('Invite deleted', 'success');
                loadInvites();
            }
            closeModal('confirm-delete-modal');
        } catch (err) {
            showToast(err.message, 'error');
        }
    });
}

// API functions
// A rejection carries its status and body along: "already running" (409) and
// "that row cannot be benchmarked" (400) are different answers to the same
// button, and only the caller knows which of them it can act on.
function apiError(response, data) {
    if (response.status === 401) {
        // The tab's token expired or was revoked (a password change signs
        // every session out): sign in again rather than show errors.
        window.mokuroAuth.clear();
        window.location.href = '/login';
    }
    const error = new Error((data && data.error) || 'Request failed');
    error.status = response.status;
    error.payload = data || {};
    return error;
}

async function apiGet(path) {
    const response = await fetch(API_BASE + path, {
        headers: { ...getAuthHeaders() },
    });
    if (!response.ok) {
        // A server that does not have this endpoint at all may answer with
        // anything; the STATUS is the part the caller acts on, so a body that
        // will not parse must not become a different error than the 404 it is.
        let data = null;
        try {
            data = await response.json();
        } catch (_) {
            data = null;
        }
        throw apiError(response, data);
    }
    return response.json();
}

async function apiPost(path, body) {
    const response = await fetch(API_BASE + path, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', ...getAuthHeaders() },
        body: JSON.stringify(body),
    });
    const data = await response.json();
    if (!response.ok) {
        throw apiError(response, data);
    }
    return data;
}

async function apiPut(path, body) {
    const response = await fetch(API_BASE + path, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json', ...getAuthHeaders() },
        body: JSON.stringify(body),
    });
    const data = await response.json();
    if (!response.ok) {
        // A rejection can say WHICH row and field it is about; carry the whole
        // body along so the caller can put the message where the mistake is
        // instead of dropping it into a toast that names nothing.
        throw apiError(response, data);
    }
    return data;
}

async function apiDelete(path) {
    const response = await fetch(API_BASE + path, {
        method: 'DELETE',
        headers: { ...getAuthHeaders() },
    });
    const data = await response.json();
    if (!response.ok) {
        throw apiError(response, data);
    }
    return data;
}

// Load users
async function loadUsers() {
    try {
        const data = await apiGet('/users');
        users = data.users;
        renderUsers();
    } catch (err) {
        usersBody.innerHTML = `<tr><td colspan="6" class="loading">Error: ${err.message}</td></tr>`;
    }
}

function renderUsers() {
    if (users.length === 0) {
        usersBody.innerHTML = '<tr><td colspan="6" class="loading">No users found</td></tr>';
        return;
    }

    usersBody.innerHTML = users.map(user => `
        <tr>
            <td>${escapeHtml(user.username)}</td>
            <td>${escapeHtml(user.role)}</td>
            <td><span class="badge ${getBadgeClass(user.status)}">${user.status}</span></td>
            <td>${user.notes ? escapeHtml(truncate(user.notes, 60)) : '-'}</td>
            <td>${formatDate(user.created_at)}</td>
            <td class="actions">
                ${user.status !== 'deleted' ? `
                    <button class="btn btn--secondary btn--sm" onclick="showChangeRole('${escapeHtml(user.username)}', '${escapeHtml(user.role)}')">
                        Role
                    </button>
                    <button class="btn btn--secondary btn--sm" onclick="showEditNotes('${escapeHtml(user.username)}')">
                        Notes
                    </button>
                    ${user.status === 'pending' ? `
                        <button class="btn btn--primary btn--sm" onclick="approveUser('${escapeHtml(user.username)}')">
                            Approve
                        </button>
                    ` : ''}
                    ${user.status === 'active' ? `
                        <button class="btn btn--secondary btn--sm" onclick="disableUser('${escapeHtml(user.username)}')">
                            Disable
                        </button>
                    ` : ''}
                    <button class="btn btn--danger btn--sm" onclick="confirmDeleteUser('${escapeHtml(user.username)}')">
                        Delete
                    </button>
                ` : ''}
            </td>
        </tr>
    `).join('');
}

// Load invites
async function loadInvites() {
    try {
        const data = await apiGet('/invites');
        invites = data.invites;
        renderInvites();
    } catch (err) {
        invitesBody.innerHTML = `<tr><td colspan="7" class="loading">Error: ${err.message}</td></tr>`;
    }
}

function renderInvites() {
    if (invites.length === 0) {
        invitesBody.innerHTML = '<tr><td colspan="7" class="loading">No invites found</td></tr>';
        return;
    }

    invitesBody.innerHTML = invites.map(invite => `
        <tr>
            <td><code class="code">${escapeHtml(invite.code)}</code></td>
            <td>${escapeHtml(invite.role)}</td>
            <td><span class="badge ${getBadgeClass(invite.status)}">${invite.status}</span></td>
            <td>${formatDate(invite.expires_at)}</td>
            <td>${invite.used_by ? escapeHtml(invite.used_by) : '-'}</td>
            <td>${invite.invited_by ? escapeHtml(invite.invited_by) : '-'}</td>
            <td class="actions">
                ${invite.status === 'valid' ? `
                    <button class="btn btn--secondary btn--sm" onclick="copyInviteCode('${escapeHtml(invite.code)}')">
                        Copy
                    </button>
                ` : ''}
                <button class="btn btn--danger btn--sm" onclick="confirmDeleteInvite('${escapeHtml(invite.code)}')">
                    Delete
                </button>
            </td>
        </tr>
    `).join('');
}

// User actions
function showChangeRole(username, currentRole) {
    document.getElementById('change-role-username').value = username;
    document.getElementById('change-role-user-display').textContent = username;
    const select = document.getElementById('change-role-select');
    // A role this menu does not list would leave it BLANK, and submitting a
    // blank select sends no role at all.
    const known = Array.from(select.options).some((option) => option.value === currentRole);
    select.value = known ? currentRole : select.options[0].value;
    openModal('change-role-modal');
}

function showEditNotes(username) {
    const user = users.find((u) => u.username === username);
    document.getElementById('edit-notes-username').value = username;
    document.getElementById('edit-notes-user-display').textContent = username;
    document.getElementById('edit-notes-text').value = user?.notes || '';
    openModal('edit-notes-modal');
}

async function approveUser(username) {
    try {
        await apiPost(`/users/${encodeURIComponent(username)}/approve`, {});
        showToast('User approved', 'success');
        loadUsers();
    } catch (err) {
        showToast(err.message, 'error');
    }
}

async function disableUser(username) {
    try {
        await apiPost(`/users/${encodeURIComponent(username)}/disable`, {});
        showToast('User disabled', 'success');
        loadUsers();
    } catch (err) {
        showToast(err.message, 'error');
    }
}

function confirmDeleteUser(username) {
    const modal = document.getElementById('confirm-delete-modal');
    modal.dataset.deleteType = 'user';
    modal.dataset.deleteId = username;
    document.getElementById('confirm-delete-message').textContent =
        `Are you sure you want to delete user "${username}"? This cannot be undone.`;
    openModal('confirm-delete-modal');
}

// Invite actions
function copyInviteCode(code) {
    navigator.clipboard.writeText(code).then(() => {
        showToast('Copied to clipboard', 'success');
    }).catch(() => {
        // Fallback for older browsers
        const input = document.createElement('input');
        input.value = code;
        document.body.appendChild(input);
        input.select();
        document.execCommand('copy');
        document.body.removeChild(input);
        showToast('Copied to clipboard', 'success');
    });
}

function confirmDeleteInvite(code) {
    const modal = document.getElementById('confirm-delete-modal');
    modal.dataset.deleteType = 'invite';
    modal.dataset.deleteId = code;
    document.getElementById('confirm-delete-message').textContent =
        `Are you sure you want to delete this invite code? This cannot be undone.`;
    openModal('confirm-delete-modal');
}

// ============================================
// Processors: the machines doing this library's OCR (spec section 6)
// ============================================

const PROCESSOR_POLL_MS = 15000;
let processorPollTimer = null;
// The per-machine throughput the Processors card last got: the generation
// cards' History reads its pages a minute from here too.
let procSpeed = [];

function processorClock(epochSeconds, withDate) {
    if (typeof epochSeconds !== 'number' || !isFinite(epochSeconds) || epochSeconds <= 0) return '—';
    const when = new Date(epochSeconds * 1000);
    return withDate ? when.toLocaleString() : when.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

function renderProcessors(data) {
    const body = document.getElementById('processors-body');
    const hint = document.getElementById('processors-hint');
    const hold = document.getElementById('processors-hold');
    const failed = document.getElementById('processor-failed-logins');
    const failedList = document.getElementById('processor-failed-list');
    const failedSummary = document.getElementById('processor-failed-summary');
    if (!body || !hint || !failed || !failedList || !hold) return;
    const rows = data.processors || [];
    procSpeed = data.speed || [];
    body.innerHTML = processorRowsHtml(rows, procSpeed);
    // The cards' History lines add these numbers up: refreshed in place.
    if (genRows.length) benchRefreshAll();
    const remote = rows.filter((p) => !p.local).length;
    const last = data.last_disconnect;
    if (remote === 0 && !data.local_processing) {
        hint.textContent = 'This server does no OCR of its own; the queue holds until a processor logs in.';
    } else if (remote === 0) {
        hint.textContent = 'This server does its own OCR. Run `mokuro-bunko processor serve` on another ' +
            'machine, logged in with an account whose role is Processor, to add one.';
    } else {
        hint.textContent = remote + ' processor' + (remote === 1 ? '' : 's') + ' connected' +
            (data.local_processing ? ', beside this server\'s own hardware.' : '.');
    }
    const held = data.processing_hold;
    if (held) {
        hold.hidden = false;
        hold.textContent = 'No processor connected since ' + processorClock(held.since, false) +
            (held.last ? ' — last: ' + held.last.name + ', disconnected ' +
                processorClock(held.last.disconnected_at, false) : '') +
            '. The queue is holding.';
    } else if (remote === 0 && !data.local_processing && last) {
        hold.hidden = false;
        hold.textContent = 'No processor connected — last: ' + last.name + ', disconnected ' +
            processorClock(last.at, false) + '. The queue is holding.';
    } else {
        hold.hidden = true;
        hold.textContent = '';
    }
    const refusals = data.failed_logins || [];
    failed.hidden = refusals.length === 0;
    if (failedSummary) failedSummary.textContent = genCount(refusals.length, 'refused login');
    failedList.innerHTML = refusals.map((f) =>
        '<li><span class="mono">' + escapeHtml(f.username) + '</span> — ' + escapeHtml(f.reason) +
        ' (' + escapeHtml(processorClock(f.at, true)) + ')</li>'
    ).join('');
}

// While the library holds a processor for failing archive downloads: when
// that ends and why, one line under its name. Nothing otherwise -- the
// archive tallies (rate, resumed, returned) are not on the card.
function processorTransfer(transfer) {
    if (!transfer) return '';
    const now = Date.now() / 1000;
    if (typeof transfer.held_until === 'number' && transfer.held_until > now) {
        return '<div class="processor-transfer processor-transfer--held">held until ' +
            escapeHtml(processorClock(transfer.held_until, false)) + ' — downloads failing' +
            (transfer.held_error ? ' (' + escapeHtml(transfer.held_error) + ')' : '') + '</div>';
    }
    return '';
}

// Rows whose runner keeps failing to START on this machine: the library
// spaces the attempts out (every poll interval x4, up to an hour) instead of
// retrying every scan. One line each: which row, the next try, and why.
function processorCannotStart(rows) {
    if (!Array.isArray(rows) || !rows.length) return '';
    return rows.map((row) =>
        '<div class="processor-transfer processor-transfer--held">' +
        escapeHtml(row.generation) + ' cannot start here — next try ' +
        escapeHtml(processorClock(row.until, false)) +
        (row.error ? ' (' + escapeHtml(row.error) + ')' : '') + '</div>'
    ).join('');
}

// Pages a minute, as the Processors card shows them.
function processorPpm(value) {
    if (typeof value !== 'number' || !isFinite(value) || value <= 0) return '—';
    return value >= 10 ? String(Math.round(value)) : value.toFixed(1).replace(/\.0$/, '');
}

// "just now", "12 min ago", "2 h ago", "6 d ago": short enough for a table
// cell. The full date and time rides along as the element's title.
function agoShort(epochSeconds) {
    if (typeof epochSeconds !== 'number' || !isFinite(epochSeconds) || epochSeconds <= 0) return '';
    const seconds = Math.max(0, Math.round(Date.now() / 1000 - epochSeconds));
    if (seconds < 90) return 'just now';
    const minutes = Math.round(seconds / 60);
    if (minutes < 90) return minutes + ' min ago';
    const hours = Math.round(minutes / 60);
    if (hours < 36) return hours + ' h ago';
    return Math.round(hours / 24) + ' d ago';
}

// One row per machine: every connected processor (this server first, as the
// server lists it), then every machine that has numbers but is not connected
// now. The speed list files this server under "local".
function processorRowsHtml(processors, speed) {
    const layersOf = {};
    speed.forEach((machine) => {
        layersOf[machine.local ? '\u0000local' : machine.name] = machine;
    });
    const html = processors.map((p) => {
        const key = p.local ? '\u0000local' : p.name;
        const machine = layersOf[key];
        delete layersOf[key];
        return processorRowHtml(p, machine || null);
    });
    Object.keys(layersOf).forEach((key) => {
        const machine = layersOf[key];
        if (!(machine.layers || []).length) return;
        html.push(processorRowHtml(null, machine));
    });
    return html.join('');
}

// A machine's CPU and GPU, "Threadripper (48 cores) · RTX 4090".
function processorHardware(host) {
    return host ? [host.cpu, host.gpu].filter(Boolean).join(' · ') : '';
}

// `p` is the connected processor (null for a machine that is only known by
// its numbers); `machine` its entry in the speed list (null when it has none).
function processorRowHtml(p, machine) {
    const local = p ? !!p.local : !!machine.local;
    const name = p ? p.name : (local ? 'this server' : machine.name);
    const online = p ? true : !!machine.connected;
    // The GPU is in the hardware column, so the name is the bare name
    // rather than the "tower (RTX 4090)" label. What a connected processor
    // registered with, else what the speed list carries: this server's own
    // (probed once by the server) and an offline machine's as it last
    // registered.
    const host = processorHardware(p && p.host) || processorHardware(machine && machine.host);
    // An empty catalog is a processor whose environments are still being
    // installed: connected, but offered nothing yet.
    const installing = !!(p && p.installing);
    return (
        '<tr data-processor="' + escapeHtml(name) + '"' + (online ? '' : ' class="processors-row--offline"') + '>' +
        '<td class="processors-machine">' +
        '<span class="processors-machine__name">' + escapeHtml(name) + '</span>' +
        (online ? '' : ' <span class="processors-machine__offline">offline</span>') +
        (installing ? ' <span class="processors-machine__installing">installing</span>' : '') +
        (p && !local
            ? '<div class="processors-machine__since" title="' +
              escapeHtml('connected ' + processorClock(p.connected_since, true)) + '">connected</div>'
            : '') +
        (p ? processorTransfer(p.transfer) + processorCannotStart(p.cannot_start) : '') +
        '</td>' +
        '<td class="processors-host" data-label="Hardware">' + escapeHtml(host) + '</td>' +
        '<td class="processors-rates-cell" data-label="Pages/min">' + processorRatesHtml(machine) + '</td>' +
        '</tr>'
    );
}

// Per generation this machine has run: ONE pages-a-minute figure. It is the
// REAL throughput (pages of its recent finished volumes over their wall
// time -- never a fitted rate); until the machine has finished a volume of
// the layer, its own benchmark fills in, marked "from benchmark" on hover.
// Admin only: the public queue page is never sent a machine's own number.
function processorRatesHtml(machine) {
    const layers = (machine && machine.layers) || [];
    if (!layers.length) return '<span class="processors-rates__none">—</span>';
    return '<ul class="processors-rates">' + layers.map((layer) => {
        const real = processorPpm(layer.pages_per_minute);
        const bench = real === '—' ? processorPpm(layer.bench_pages_per_minute) : '—';
        const figure = bench !== '—'
            ? '<span class="processors-rates__real processors-rates__real--bench" title="from benchmark">' +
              escapeHtml(bench) + '</span>'
            : '<span class="processors-rates__real"' +
              (layer.volumes ? ' title="' + escapeHtml('over the last ' + genCount(layer.volumes, 'volume')) + '"' : '') +
              '>' + escapeHtml(real) + '</span>';
        return (
            '<li data-speed-generation="' + escapeHtml(layer.generation || '') + '">' +
            '<span class="processors-rates__gen mono">' + escapeHtml(layer.generation || '') + '</span>' +
            figure + '</li>'
        );
    }).join('') + '</ul>';
}

async function loadProcessors() {
    try {
        renderProcessors(await apiGet('/processors'));
    } catch (_) {
        // A server that predates processors has no such endpoint: the card
        // simply stays empty.
    }
}

function startProcessorPolling() {
    stopProcessorPolling();
    loadProcessors();
    processorPollTimer = window.setInterval(loadProcessors, PROCESSOR_POLL_MS);
}

function stopProcessorPolling() {
    if (processorPollTimer !== null) {
        window.clearInterval(processorPollTimer);
        processorPollTimer = null;
    }
}

// ============================================
// Settings Tab
// ============================================

async function loadSettings() {
    try {
        const data = await apiGet('/settings');
        // Registration
        document.getElementById('settings-reg-mode').value = data.registration?.mode || 'self';
        document.getElementById('settings-reg-role').value = data.registration?.default_role || 'registered';
        const allowAnonBrowse =
            data.registration?.allow_anonymous_browse ?? !(data.registration?.require_login ?? false);
        const allowAnonDownload =
            data.registration?.allow_anonymous_download ?? !(data.registration?.require_login ?? false);
        document.getElementById('settings-anon-webdav').checked = !!(allowAnonBrowse && allowAnonDownload);
        // CORS
        document.getElementById('settings-cors-enabled').checked = data.cors?.enabled ?? true;
        corsOrigins = data.cors?.allowed_origins || [];
        renderCorsOrigins();
        // Catalog
        document.getElementById('settings-catalog-enabled').checked = data.catalog?.enabled ?? false;
        document.getElementById('settings-catalog-as-home').checked = data.catalog?.use_as_homepage ?? false;
        setReaderUrl(data.catalog?.reader_url || 'https://reader.mokuro.app');
        // Queue
        document.getElementById('settings-queue-show-nav').checked = data.queue?.show_in_nav ?? false;
        document.getElementById('settings-queue-public').checked = data.queue?.public_access ?? true;
        document.getElementById('settings-queue-display').value = data.queue?.display || 'normal';
        // OCR
        document.getElementById('settings-ocr-interval').value = data.ocr?.poll_interval || 30;
        renderOcrRuntimeStatus(data.ocr_runtime || {}, data.ocr || {});
    } catch (err) {
        showToast('Failed to load settings: ' + err.message, 'error');
    }
    loadGenerations();
}

// The Environment block is one line (the backend) with the paths and the
// active rows folded under Details, and each fact has to read right in every
// state the server can be in -- including the states where it cannot answer.
// "Installed" is three answers, not two (installed, not installed, not
// reported), and a backend of `skip` makes the rest beside the point; a
// missing path is a thing unsaid, never a dash with a clause bolted onto it.
function renderOcrRuntimeStatus(runtime, ocrConfig) {
    setKvValue('ocr-backend', ocrBackendSentence(runtime, ocrConfig), false);
    setKvValue('ocr-env-path', runtime.env_path || 'not reported', !!runtime.env_path);
    setKvValue('ocr-engines-env', ocrEnginesEnvText(runtime), !!runtime.engines_env_path);
    const active = ocrActiveNames(runtime);
    setKvValue('ocr-active-engines', active.text, active.mono);
    document.getElementById('ocr-cli-hint').innerHTML = codeSpansHtml(
        runtime.cli_hint || 'To change the backend, restart the server with --ocr set to auto, cuda, rocm, cpu or skip.');
    document.getElementById('ocr-driver-hint').innerHTML = codeSpansHtml(runtime.driver_hint || '');
}

// Server-written hint text: a `backticked` span is a command, set as code
// rather than shown with its backticks. Everything is escaped.
function codeSpansHtml(text) {
    return String(text).split('`').map((part, i) =>
        i % 2 ? '<code>' + escapeHtml(part) + '</code>' : escapeHtml(part)).join('');
}

// Paths are monospace; the sentences that stand in for a path when there is
// none are not.
function setKvValue(id, text, mono) {
    const el = document.getElementById(id);
    if (!el) return;
    el.textContent = text;
    el.classList.toggle('mono', !!mono);
}

const BACKEND_LABEL = { auto: 'Auto', rocm: 'ROCm', cuda: 'CUDA', cpu: 'CPU', skip: 'Skip' };

function backendLabel(name) {
    return BACKEND_LABEL[String(name).toLowerCase()] || String(name);
}

// The one line after "Backend:". Short, but every state still reads as what
// it is: off (and why), not reported, not installed, installed, or a
// configured backend that is not the installed one.
function ocrBackendSentence(runtime, ocrConfig) {
    const configured = runtime.configured_backend || ocrConfig.backend || 'auto';
    const installedBackend = runtime.installed_backend || null;
    const knows = Object.prototype.hasOwnProperty.call(runtime, 'installed');
    const chosen = backendLabel(configured);
    let line;
    let off = false;
    if (runtime.local_processing === false) {
        off = true;
        line = configured === 'skip'
            ? 'off here (set to skip) — processors read every volume'
            : 'off here (local processing off) — processors read every volume';
    } else if (configured === 'skip') {
        off = true;
        line = 'off (set to skip) — this server reads no volumes';
    } else if (!knows) {
        line = chosen + ' — install state not reported';
    } else if (!runtime.installed) {
        line = chosen + ' — not installed yet';
    } else if (!installedBackend || installedBackend === configured) {
        line = chosen;
    } else if (configured === 'auto') {
        line = 'Auto → ' + backendLabel(installedBackend);
    } else {
        line = chosen + ' set, but ' + backendLabel(installedBackend) + ' installed';
    }
    const supported = (runtime.supported_backends || []).map(backendLabel).join(', ');
    return supported && !off ? line + ' (available: ' + supported + ')' : line;
}

function ocrEnginesEnvText(runtime) {
    const path = runtime.engines_env_path || '';
    if (!path) return 'not reported';
    if (runtime.engines_installed === false) return path + ' (not installed yet; installs on next start)';
    if (runtime.engines_installed === true && runtime.detector_ready === false) {
        return path + ' (detector extras install on next start)';
    }
    return path;
}

// The server names whatever it is actually running this session; the page
// prints it rather than deriving it, so a name the UI has never seen (a
// generation added by hand in the YAML) still shows up here. It may send
// names or whole rows, and older servers sent engine ids.
function ocrActiveNames(runtime) {
    const raw = runtime.active_generations || runtime.generations ||
        runtime.active_engines || runtime.engines || [];
    const names = (Array.isArray(raw) ? raw : [])
        .map((item) => (typeof item === 'string' ? item : (item && (item.name || item.id)) || ''))
        .filter(Boolean);
    if (names.length) return { text: names.join(', '), mono: true };
    return { text: runtime.available === false ? 'not reported' : 'none', mono: false };
}

function setReaderUrl(url) {
    const presets = [
        'https://reader.mokuro.app',
        'http://localhost:5173',
        'https://mokuro-reader-tan.vercel.app',
    ];
    const customInput = document.getElementById('reader-url-custom-input');
    if (presets.includes(url)) {
        document.querySelector(`input[name="reader-url"][value="${url}"]`).checked = true;
        customInput.style.display = 'none';
    } else {
        document.getElementById('reader-url-custom').checked = true;
        customInput.style.display = '';
        customInput.value = url;
    }
}

function getReaderUrl() {
    const selected = document.querySelector('input[name="reader-url"]:checked');
    if (!selected) return 'https://reader.mokuro.app';
    if (selected.value === 'custom') {
        return document.getElementById('reader-url-custom-input').value.trim() || 'https://reader.mokuro.app';
    }
    return selected.value;
}

// Toggle custom URL input visibility
document.addEventListener('DOMContentLoaded', () => {
    initGenerations();
    document.querySelectorAll('input[name="reader-url"]').forEach(radio => {
        radio.addEventListener('change', () => {
            const customInput = document.getElementById('reader-url-custom-input');
            customInput.style.display = radio.value === 'custom' && radio.checked ? '' : 'none';
        });
    });
});

async function saveRegistrationSettings() {
    try {
        const allowAnonymousWebdav = document.getElementById('settings-anon-webdav').checked;
        await apiPut('/settings/registration', {
            mode: document.getElementById('settings-reg-mode').value,
            default_role: document.getElementById('settings-reg-role').value,
            allow_anonymous_browse: allowAnonymousWebdav,
            allow_anonymous_download: allowAnonymousWebdav,
        });
        showToast('Registration settings saved', 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
}

function renderCorsOrigins() {
    const list = document.getElementById('cors-origins-list');
    list.innerHTML = corsOrigins.map((origin, i) => `
        <div class="origin-item">
            <span class="origin-item__text">${escapeHtml(origin)}</span>
            <button class="btn btn--danger btn--sm" onclick="removeCorsOrigin(${i})">Remove</button>
        </div>
    `).join('');
}

function addCorsOrigin() {
    const input = document.getElementById('cors-new-origin');
    const origin = input.value.trim();
    if (origin && !corsOrigins.includes(origin)) {
        corsOrigins.push(origin);
        renderCorsOrigins();
        input.value = '';
    }
}

function removeCorsOrigin(index) {
    corsOrigins.splice(index, 1);
    renderCorsOrigins();
}

async function saveCorsSettings() {
    try {
        await apiPut('/settings/cors', {
            enabled: document.getElementById('settings-cors-enabled').checked,
            allowed_origins: corsOrigins,
        });
        showToast('CORS settings saved', 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
}

async function saveCatalogSettings() {
    try {
        await apiPut('/settings/catalog', {
            enabled: document.getElementById('settings-catalog-enabled').checked,
            use_as_homepage: document.getElementById('settings-catalog-as-home').checked,
            reader_url: getReaderUrl(),
        });
        showToast('Catalog settings saved', 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
}

// The scan interval is all `PUT /settings/ocr` still carries; engines,
// detector and patch budget moved into the generation rows below.
async function saveOcrSettings() {
    try {
        const result = await apiPut('/settings/ocr', {
            poll_interval: parseInt(document.getElementById('settings-ocr-interval').value, 10),
        });
        showToast(liveApplyMessage(result, 'Scan interval saved'), result?.restart_required ? 'warning' : 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
}

// The four live-apply outcome fields every OCR write answers with, in the one
// sentence the page has always shown for them.
function liveApplyMessage(result, saved) {
    // Applied, but something only a restart can install here: the rows that
    // need it run on a connected processor meanwhile, and the reason says so.
    if (result?.applied && result.restart_required && result.reason) {
        return saved + ' and applied; ' + result.reason;
    }
    if (result?.applied) return saved + ' and applied';
    if (result?.installing) return saved + '; ' + (result.reason || 'the engine environment is installing');
    if (result?.restart_required) {
        return saved + '; ' + (result.reason || 'it applies when the server restarts');
    }
    return saved;
}

async function saveQueueSettings() {
    try {
        await apiPut('/settings/queue', {
            show_in_nav: document.getElementById('settings-queue-show-nav').checked,
            public_access: document.getElementById('settings-queue-public').checked,
            display: document.getElementById('settings-queue-display').value,
        });
        showToast('Queue settings saved', 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
}

// ============================================
// OCR generations
//
// A generation is one sidecar file per volume: a name, an engine, a detector,
// and the pool widths the staged runner uses to produce it. The list IS the
// run order, so reordering is a first-class edit and not a display option.
//
// Interaction model follows the CORS origins list above: a module-level array,
// a render() that writes innerHTML, mutating helpers, one Save that PUTs the
// whole ordered list. The one departure is that typing in a name never
// re-renders (that would drop the caret); only structural edits do.
// ============================================

const GEN_NAME_PATTERN_FALLBACK = '^[a-z0-9][a-z0-9-]{0,31}$';
const GEN_NAME_MAX = 32;
const GEN_SAMPLE_VOLUME = 'Volume 01';

// Why a generation steps over a volume, in the words the Queue page uses for
// the same volumes: it is one fact with one fix, and it is not an error.
const GEN_SKIPPED_WHY =
    'These volumes were uploaded with pages missing — the archive holds fewer images than its ' +
    '.mokuro names. They keep the OCR they arrived with and get no additional layers. ' +
    'Replace the file with a complete volume to have them generated.';

let genRows = [];
let genCatalog = {
    engines: [],
    detectors: [],
    devices: [],
    patch_budgets: [],
    precision_modes: [],
    precision_default: 'auto-accuracy',
    name_pattern: GEN_NAME_PATTERN_FALLBACK,
    reserved_names: [],
    reserved_prefixes: [],
};
let genSavedJson = null;
let genLoadError = null;
let genSeq = 0;
// The connected processors (spec section 4): each row's pools can be set per
// machine, and a benchmark run on any of them. Empty on a single-machine
// install, which then looks exactly as it always has.
let genProcessors = [];
let genLocalProcessing = true;
// `ocr.autobench`: whether a row nobody configured is benchmarked here first.
let genAutobench = true;
// The last rejection from the server, parked on the row it names so the
// message sits next to the field instead of vanishing with a toast.
let genServerError = null;
let genConfirmRemove = -1;

function initGenerations() {
    const list = document.getElementById('gen-list');
    if (!list) return;
    list.addEventListener('input', onGenInput);
    list.addEventListener('change', onGenChange);
    list.addEventListener('click', onGenClick);
    // `toggle` does not bubble, so the trials disclosure is watched in the
    // capture phase; without it a poll's refresh would close it under the
    // reader's hands.
    list.addEventListener('toggle', onGenToggle, true);
    document.addEventListener('visibilitychange', onBenchVisibility);
    document.getElementById('gen-add-btn').addEventListener('click', addGeneration);
    document.getElementById('gen-revert-btn').addEventListener('click', revertGenerations);
    document.getElementById('gen-save-btn').addEventListener('click', saveGenerations);
}

async function loadGenerations() {
    try {
        const data = await apiGet('/ocr/generations');
        genLoadError = null;
        adoptGenerations(data);
    } catch (err) {
        genLoadError = err.message;
        renderGenerations();
    }
}

function adoptGenerations(data) {
    genCatalog = {
        engines: (data.catalog && data.catalog.engines) || [],
        detectors: (data.catalog && data.catalog.detectors) || [],
        devices: (data.catalog && data.catalog.devices) || [],
        patch_budgets: (data.catalog && data.catalog.patch_budgets) || [],
        precision_modes: (data.catalog && data.catalog.precision_modes) || [],
        precision_default: (data.catalog && data.catalog.precision_default) || 'auto-accuracy',
        name_pattern: (data.catalog && data.catalog.name_pattern) || GEN_NAME_PATTERN_FALLBACK,
        reserved_names: (data.catalog && data.catalog.reserved_names) || [],
        reserved_prefixes: (data.catalog && data.catalog.reserved_prefixes) || [],
    };
    genProcessors = (data.processors || []).filter((p) => !p.local);
    genLocalProcessing = data.local_processing !== false;
    genAutobench = data.autobench !== false;
    const open = {};
    const machines = {};
    genRows.forEach((row) => {
        if (row.id) {
            open[row.id] = row.open;
            machines[row.id] = row.machine;
        }
    });
    genRows = (data.generations || []).map((g) => genRowFromServer(g, open, machines));
    // A name that already equals what engine+detector would derive keeps
    // following them; anything else is the user's and is left alone.
    genRows.forEach((row) => { row.nameEdited = row.name !== genDefaultName(row); });
    genConfirmRemove = -1;
    genServerError = null;
    genSavedJson = JSON.stringify(genPutRows());
    adoptBenchSummaries(data.generations || []);
    renderGenerations();
    // The list carries only the last FINISHED benchmark, so a run that is
    // still going is invisible in it: ask each row directly, which is also
    // what makes a reload mid-run pick the progress back up.
    benchProbeAll();
    // The volume counts are worked out in the background on a large library:
    // the list comes without them, and they are filled in when ready.
    genStatsPending = !!data.stats_pending;
    if (genStatsPending) loadGenerationStats();
}

let genStatsTimer = null;
// True while the rows' volume counts are still being worked out.
let genStatsPending = false;

// Fill in the rows' volume counts (done / total / skipped / by machine) once
// the server has them. Only those fields are touched, so nothing being
// edited is disturbed; asks again every few seconds while they are pending.
async function loadGenerationStats(attempt) {
    clearTimeout(genStatsTimer);
    const tries = attempt || 0;
    let data;
    try {
        data = await apiGet('/ocr/generations/stats');
    } catch (err) {
        return;
    }
    if (data.stats_pending) {
        if (tries < 60) genStatsTimer = setTimeout(() => loadGenerationStats(tries + 1), 3000);
        return;
    }
    genStatsPending = false;
    const byId = data.generations || {};
    genRows.forEach((row) => {
        const stats = row.id ? byId[row.id] : null;
        if (!stats) return;
        row.volumesDone = typeof stats.volumes_done === 'number' ? stats.volumes_done : null;
        row.volumesTotal = typeof stats.volumes_total === 'number' ? stats.volumes_total : null;
        row.volumesSkipped = typeof stats.volumes_skipped === 'number' ? stats.volumes_skipped : 0;
        row.volumesByMachine = Object.assign({}, stats.volumes_by_machine);
    });
    renderGenerations();
}

function genRowFromServer(g, openById, machineById) {
    const pools = g.pools || {};
    const kept = g.id && machineById ? machineById[g.id] : null;
    return {
        id: g.id || null,
        key: 'g' + (++genSeq),
        name: g.name || '',
        primary: !!g.primary,
        enabled: g.enabled !== false,
        engine: g.engine || '',
        detector: g.detector || null,
        patch_budget: typeof g.patch_budget === 'number' ? g.patch_budget : null,
        // The row's ONE precision mode, for every machine (absent: the default).
        precision: typeof g.precision === 'string' && g.precision ? g.precision : genDefaultMode(),
        pools: {
            stage_workers: Object.assign({}, pools.stage_workers),
            queue_capacity: Object.assign({}, pools.queue_capacity),
            stage_device: Object.assign({}, pools.stage_device),
        },
        sidecar: g.sidecar || null,
        effectiveDetector: g.effective_detector || null,
        detectorLocked: !!g.detector_locked,
        patchBudgetApplies: !!g.patch_budget_applies,
        // What every mode resolves to on each machine, as the server worked
        // it out for the SAVED engine, and the saved state's hold reason.
        precisionOn: g.precision_on || null,
        precisionHold: typeof g.precision_hold === 'string' && g.precision_hold ? g.precision_hold : null,
        // Why the row no longer runs (its engine was removed), or null.
        retired: typeof g.retired === 'string' && g.retired ? g.retired : null,
        savedEngine: g.engine || '',
        savedPrecision: typeof g.precision === 'string' && g.precision ? g.precision : genDefaultMode(),
        road: g.road || null,
        stages: Array.isArray(g.stages) ? g.stages : [],
        volumesDone: typeof g.volumes_done === 'number' ? g.volumes_done : null,
        volumesTotal: typeof g.volumes_total === 'number' ? g.volumes_total : null,
        volumesSkipped: typeof g.volumes_skipped === 'number' ? g.volumes_skipped : 0,
        // Per machine: this row's sidecars on disk now that it wrote (the
        // server's provenance records). What the History line's share is.
        volumesByMachine: Object.assign({}, g.volumes_by_machine),
        congestion: g.congestion || null,
        nameEdited: true,
        open: !!(g.id && openById && openById[g.id]),
        whyOpen: false,
        historyOpen: false,
        // Set while the server is working out this row's stages after an
        // engine, detector or device change: the stage keys are its to name.
        stagesStale: false,
        // Which derive answer is still wanted, so a slow one for an older
        // edit cannot overwrite a newer one.
        deriveSeq: 0,
        // Per processor (spec section 4): what is stored for each machine,
        // what is being edited for it, the stages it would run, and what it
        // has measured. `machine` is the one the pools table shows.
        processorPools: Object.assign({}, g.processor_pools),
        processorRuns: Object.assign({}, g.processor_runs),
        // This server's own lifetime count of the row, like a processor's
        // `processor_runs` entry; null while it has read none.
        localRuns: g.local_runs || null,
        processorBench: Object.assign({}, g.processor_bench),
        processorCongestion: Object.assign({}, g.processor_congestion),
        // This server's own auto-benchmark of a row nobody configured by hand:
        // the pools it runs here instead of the derived defaults, and the
        // benchmark that found them (`OCRWorker._local_pools`).
        localPools: g.local_pools || null,
        localBench: g.local_bench || null,
        machinePools: {},
        // Each processor's own table as the server worked it out with that
        // machine's devices (`processor_stages`), so switching to it shows
        // its Device select at once instead of waiting on a derive.
        machineStages: Object.assign({}, g.processor_stages),
        machine: kept || genDefaultMachine(),
    };
}

// ---- which machine a card is showing ----------------------------------

// The card's Machine select: every machine at once ("All machines", the
// default), this server, or one connected processor. Offered only while
// there is more than one machine to choose from, and only on a saved row (a
// processor's pools are filed under the row's id).
const GEN_ALL = 'all';

function genHasMachineChoice(row) {
    return genProcessors.length > 0 && !!(row && row.id);
}

function genDefaultMachine() {
    return genProcessors.length ? GEN_ALL : 'local';
}

// 'all', 'local' or a connected processor's name.
function genMachine(row) {
    if (!genHasMachineChoice(row)) return 'local';
    const wanted = row.machine || GEN_ALL;
    if (wanted === GEN_ALL || wanted === 'local') return wanted;
    return genProcessors.some((p) => p.name === wanted) ? wanted : GEN_ALL;
}

function genIsAll(row) {
    return genMachine(row) === GEN_ALL;
}

// The machine whose pools the row's editing state is about. All machines
// has no pools table of its own (the card says to pick a machine); anything
// that still needs one -- a derive after an engine change, the spec a save
// validates -- works on the row's own table, which is this server's.
function genPoolMachine(row) {
    const machine = genMachine(row);
    return machine === GEN_ALL ? 'local' : machine;
}

// Every machine the All view adds up: this server while it does OCR of its
// own, then every connected processor.
function genAllMachines() {
    return (genLocalProcessing ? ['local'] : []).concat(genProcessors.map((p) => p.name));
}

function genMachineLabel(name) {
    if (name === 'local') return 'this server';
    const found = genProcessors.find((p) => p.name === name);
    return found ? (found.label || found.name) : name;
}

function genClonePools(pools) {
    const source = pools || {};
    return {
        stage_workers: Object.assign({}, source.stage_workers),
        queue_capacity: Object.assign({}, source.queue_capacity),
        stage_device: Object.assign({}, source.stage_device),
    };
}

// Does a machine's stored pools say anything? Only when one of its tables
// names a stage: three empty tables are no opinion, and the server runs the
// row's own table there (`profiles.holds_pools`).
function genHoldsPools(pools) {
    if (!pools) return false;
    return ['stage_workers', 'queue_capacity', 'stage_device'].some(
        (key) => Object.keys(pools[key] || {}).length > 0);
}

// No entry for this machine: the row's own table IS its default. And table
// by table: one the machine's entry leaves empty says nothing, so the row's
// own runs for it there (`profiles.machine_pools`).
function poolsFor(row, name) {
    if (name === 'local') return row.pools;
    const stored = row.processorPools[name];
    if (!genHoldsPools(stored)) return row.pools;
    const out = {};
    ['stage_workers', 'queue_capacity', 'stage_device'].forEach((key) => {
        const table = stored[key] || {};
        out[key] = Object.keys(table).length ? table : (row.pools[key] || {});
    });
    return out;
}

// The pools one machine's table edits: the row's own, or a working copy of
// one processor's (saved with its own button, never with the list).
function genPoolsOf(row, name) {
    if (name === 'local') return row.pools;
    if (!row.machinePools[name]) row.machinePools[name] = genClonePools(poolsFor(row, name));
    return row.machinePools[name];
}

// ... and the ones the table shows right now.
function genActivePools(row) {
    return genPoolsOf(row, genPoolMachine(row));
}

function genActiveStages(row) {
    const name = genPoolMachine(row);
    return name === 'local' ? row.stages : (row.machineStages[name] || null);
}

// The spec a derive or a benchmark sends: the row as edited, with the pools
// of the machine it is for.
function genSpecFor(row, name) {
    const spec = genRowSpec(row);
    if (name !== 'local') {
        const pools = genPoolsOf(row, name);
        spec.pools = {
            stage_workers: genCleanPool(pools.stage_workers),
            queue_capacity: genCleanPool(pools.queue_capacity),
            stage_device: genCleanDevices(pools.stage_device),
        };
    }
    return spec;
}

// ---- catalog lookups ----------------------------------------------------

function genEngine(id) {
    return genCatalog.engines.find((e) => e.id === id) || null;
}

// An engine OWNS DETECTION when the detector select is not the user's to set:
// it reads a whole volume in one pass outside the staged pipeline
// (monolithic), it is a process of its own that finds its own text (served),
// or it brings a detector it was trained with (own_detector).
function genOwnsDetection(spec) {
    return !!spec && (!!spec.monolithic || !!spec.served || !!spec.own_detector);
}

function genDetectorLocked(row) {
    const spec = genEngine(row.engine);
    if (!spec) return row.detectorLocked;
    return genOwnsDetection(spec);
}

function genEffectiveDetector(row) {
    const spec = genEngine(row.engine);
    if (!spec) return row.effectiveDetector;
    // A detector behind the engine's own command line is one this server
    // never names, so naming one of ours here would be a guess.
    if (spec.monolithic || spec.served) return null;
    if (spec.own_detector) return spec.own_detector;
    return row.detector || null;
}

function genUsesPatchBudget(row) {
    const spec = genEngine(row.engine);
    if (!spec) return row.patchBudgetApplies;
    return !!spec.patch_budget;
}

// ---- the row's precision mode ---------------------------------------------

// One mode per row, applying to every machine: this server and every
// processor. The server resolves it on each machine's card and says what it
// found (`precision_on`); nothing here decides a precision of its own.
function genDefaultMode() {
    return genCatalog.precision_default || 'auto-accuracy';
}

// The modes a row's engine offers: none for an engine that fixes its own
// precision (ppocr-manga), and never bf16 for mokuro -- the catalog says.
function genPrecisionModes(row) {
    const spec = genEngine(row.engine);
    return spec && Array.isArray(spec.precision_modes) ? spec.precision_modes : [];
}

function genModeLabel(mode) {
    const found = (genCatalog.precision_modes || []).find((m) => m.id === mode);
    return found && found.label ? String(found.label) : String(mode);
}

// `precision_on` answers for the engine the server worked it out for -- the
// saved one. After an engine change it answers nothing until a save.
function genPrecisionOn(row) {
    if (!row.precisionOn || typeof row.precisionOn !== 'object') return null;
    return row.engine === row.savedEngine ? row.precisionOn : null;
}

// What the row's CURRENT mode resolves to on one machine, or null.
function genModeOn(row, machine, mode) {
    const on = genPrecisionOn(row);
    const table = on && on[machine];
    const entry = table && table[mode || row.precision];
    return entry && typeof entry === 'object' ? entry : null;
}

// "No connected machine can run bf16" when nobody in the All set can run the
// row's current mode (live, from `precision_on`), else the saved state's
// hold while the row still says what was saved; null when it can run.
function genPrecisionHold(row) {
    if (!genPrecisionModes(row).length) return null;
    const machines = genAllMachines();
    const entries = machines.map((machine) => genModeOn(row, machine)).filter(Boolean);
    if (entries.some((entry) => entry.eligible !== false)) return null;
    if (machines.length && entries.length === machines.length) {
        return 'No connected machine can run ' + row.precision;
    }
    const saved = row.engine === row.savedEngine && row.precision === row.savedPrecision;
    return saved ? row.precisionHold : null;
}

// One machine's line: "runs bf16 here", with a balanced or speed mode's
// benchmark numbers when it has them; "decided when it starts here" for a
// machine that did not report its card; "not eligible here: <why>".
function genResolutionText(row, machine) {
    const entry = genModeOn(row, machine);
    if (!entry) return '';
    if (entry.eligible === false) return 'not eligible here: ' + (entry.why || 'its card cannot run ' + row.precision);
    if (!entry.precision) return 'decided when it starts here';
    // A balanced/speed mode's pick is this machine's benchmark's: say where
    // it stands (`precision_on[...].bench`).
    if (entry.bench === 'pending') return 'benchmark pending';
    if (entry.bench === 'off') return 'not benchmarked (automatic benchmarks off), using ' + entry.precision;
    if (entry.bench === 'failed') return 'benchmark failed, using ' + entry.precision;
    const trials = (Array.isArray(entry.trials) ? entry.trials : []).filter((t) =>
        t && typeof t.precision === 'string' && typeof t.pages_per_second === 'number' && isFinite(t.pages_per_second));
    if (entry.bench === 'done' && trials.length) {
        // The pick first, then the others fastest first: "bf16 (benchmark: 3.1 vs 1.5 p/s)".
        const picked = trials.filter((t) => t.precision === entry.precision);
        const others = trials.filter((t) => t.precision !== entry.precision)
            .sort((a, b) => b.pages_per_second - a.pages_per_second);
        const rates = picked.concat(others).map((t) => String(Number(t.pages_per_second.toFixed(2))));
        return entry.precision + ' (benchmark: ' + rates.join(' vs ') + ' p/s)';
    }
    return 'runs ' + entry.precision + ' here';
}

function genIsMonolithic(row) {
    const spec = genEngine(row.engine);
    if (!spec) return row.road === null && row.stages.length === 0 && row.detectorLocked;
    return !!spec.monolithic;
}

// ---- names --------------------------------------------------------------

function genNameRegExp() {
    try {
        return new RegExp(genCatalog.name_pattern || GEN_NAME_PATTERN_FALLBACK);
    } catch (_) {
        return new RegExp(GEN_NAME_PATTERN_FALLBACK);
    }
}

// The engine id alone when the engine brings its own detector, else
// <engine>-<detector>; truncated to GEN_NAME_MAX with any trailing dash
// trimmed, and -2, -3, ... appended when another row already holds the name.
function genDefaultName(row) {
    const spec = genEngine(row.engine);
    const bringsOwn = !spec || genOwnsDetection(spec);
    const detector = row.detector || '';
    let stem = bringsOwn || !detector ? row.engine || '' : row.engine + '-' + detector;
    stem = stem.slice(0, GEN_NAME_MAX).replace(/-+$/, '');
    if (!stem) return '';
    const taken = genRows.filter((other) => other !== row).map((other) => other.name.trim());
    if (taken.indexOf(stem) === -1) return stem;
    for (let n = 2; n < 1000; n++) {
        const tail = '-' + n;
        const candidate = stem.slice(0, GEN_NAME_MAX - tail.length).replace(/-+$/, '') + tail;
        if (taken.indexOf(candidate) === -1) return candidate;
    }
    return stem;
}

// Mirrors the server's rules exactly: grammar, reserved names, the tr- prefix,
// and uniqueness across ALL rows whether enabled or not.
function genRowError(row) {
    const name = row.name.trim();
    if (!name) return { field: 'name', message: 'Give this generation a name — it is the file name postfix.' };
    if (!genNameRegExp().test(name)) {
        return {
            field: 'name',
            message: 'Use 1–' + GEN_NAME_MAX + ' characters: lowercase letters, digits and dashes, starting with a letter or digit. No dots, spaces, capitals or underscores.',
        };
    }
    if (genCatalog.reserved_names.indexOf(name) !== -1) {
        return { field: 'name', message: '"' + name + '" is reserved by the reader for another kind of layer. Pick another name.' };
    }
    const prefix = genCatalog.reserved_prefixes.find((p) => name.indexOf(p) === 0);
    if (prefix) {
        return { field: 'name', message: 'Names starting with "' + prefix + '" are reserved by the reader for translations. Pick another name.' };
    }
    const clash = genRows.findIndex((other) => other !== row && other.name.trim() === name);
    if (clash !== -1) {
        return { field: 'name', message: 'Generation ' + (clash + 1) + ' already uses this name. Every file name postfix must be unique.' };
    }
    if (!row.engine) return { field: 'engine', message: 'Choose an engine.' };
    if (!genDetectorLocked(row) && !row.detector) return { field: 'detector', message: 'Choose a detector.' };
    return null;
}

// The one rule that is about the list rather than a row.
function genListError() {
    const enabled = genRows.filter((row) => row.enabled);
    if (!enabled.length) return null;
    const primaries = enabled.filter((row) => row.primary);
    if (primaries.length === 1) return null;
    if (!primaries.length) {
        return 'No enabled generation is primary. Mark one — without it no volume gets the bare ' + GEN_SAMPLE_VOLUME + '.mokuro that readers open by default.';
    }
    return 'Only one enabled generation can be primary.';
}

// ---- rendering ----------------------------------------------------------

function renderGenerations(focus) {
    const list = document.getElementById('gen-list');
    if (!list) return;
    const banner = document.getElementById('gen-banner');
    const empty = document.getElementById('gen-empty');

    if (genLoadError) {
        list.innerHTML = '';
        empty.hidden = false;
        empty.textContent = 'Could not load generations: ' + genLoadError;
        banner.hidden = true;
        document.getElementById('gen-save-btn').disabled = true;
        return;
    }

    list.innerHTML = genRows.map(genRowHtml).join('');
    empty.hidden = genRows.length > 0;
    empty.textContent = 'No generations yet. Add one — until you do, no volume is read.';

    const listError = genListError();
    banner.hidden = !listError;
    banner.textContent = listError || '';

    const invalid = genRows.some((row) => genRowError(row)) || !!listError;
    const dirty = genIsDirty();
    document.getElementById('gen-save-btn').disabled = invalid;
    document.getElementById('gen-revert-btn').disabled = !dirty;
    document.getElementById('gen-dirty').hidden = !dirty;

    if (focus) {
        const el = list.querySelector(focus);
        if (el && !el.disabled) el.focus();
        else {
            const fallback = list.querySelector(focus.replace('"up"', '"down"'));
            if (fallback && !fallback.disabled) fallback.focus();
        }
    }
}

function genRowHtml(row, index) {
    const error = genRowError(row);
    const serverError = genServerError && genServerError.row === index ? genServerError : null;
    const shown = serverError || error;
    const classes = ['gen'];
    if (!row.enabled) classes.push('gen--off');
    if (shown) classes.push('gen--invalid');
    const locked = genDetectorLocked(row);
    const effective = genEffectiveDetector(row);
    const label = row.name.trim() || 'generation ' + (index + 1);

    return (
        '<li class="' + classes.join(' ') + '" data-idx="' + index + '">' +
        '<div class="gen__bar">' +
        '<span class="gen__num">' + (index + 1) + '</span>' +
        '<span class="gen__move">' +
        genIconButton('up', index, '↑', 'Move ' + label + ' up', index === 0, GEN_ORDER_TIP) +
        genIconButton('down', index, '↓', 'Move ' + label + ' down', index === genRows.length - 1, GEN_ORDER_TIP) +
        '</span>' +
        '<label class="check gen__check"><input type="checkbox" data-act="enabled" data-idx="' + index + '"' +
        (row.enabled ? ' checked' : '') + '> Enabled</label>' +
        '<label class="check gen__check" title="The primary generation writes the bare ' + GEN_SAMPLE_VOLUME + '.mokuro that readers open by default.">' +
        '<input type="radio" name="gen-primary" data-act="primary" data-idx="' + index + '"' +
        (row.primary ? ' checked' : '') + (row.enabled ? '' : ' disabled') + '> Primary</label>' +
        '<span class="gen__spacer"></span>' +
        genRemoveHtml(row, index) +
        '</div>' +
        (row.retired ? '<p class="gen-banner" role="status">Retired: ' + escapeHtml(row.retired) + '</p>' : '') +
        '<div class="gen__fields">' +
        genNameFieldHtml(row, index, shown) +
        genSelectFieldHtml(row, index, 'engine', 'Engine',
            genCatalog.engines.map((e) => ({ value: e.id, label: e.id, title: e.label || e.id })), row.engine,
            false, genCatalogLabel(genCatalog.engines, row.engine),
            shown && shown.field === 'engine') +
        genDetectorFieldHtml(row, index, locked, effective, shown) +
        (genUsesPatchBudget(row)
            ? genSelectFieldHtml(row, index, 'patch', 'Patch budget',
                genCatalog.patch_budgets.map((p) => ({ value: String(p), label: String(p) })),
                row.patch_budget === null ? '' : String(row.patch_budget),
                false, 'How much of each line ' + row.engine + ' gets to look at.', false)
            : '') +
        '</div>' +
        genMachineRowHtml(row, index) +
        genNameRowHtml(row, index, shown) +
        genHistoryHtml(row, index) +
        genBenchHtml(row, index) +
        genTuningHtml(row, index) +
        '</li>'
    );
}

// What the order means, on the arrows that change it: the one-line intro
// above the list leaves it to them.
const GEN_ORDER_TIP = 'Generations run in list order: every volume gets the top row before any gets the next, ' +
    'and the top row runs at full priority, the rest behind it.';

function genIconButton(act, index, glyph, label, disabled, tip) {
    return (
        '<button type="button" class="btn btn--secondary gen__icon" data-act="' + act + '" data-idx="' + index + '"' +
        ' aria-label="' + escapeHtml(label) + '" title="' + escapeHtml(tip ? label + ' — ' + tip : label) + '"' +
        (disabled ? ' disabled' : '') + '>' + glyph + '</button>'
    );
}

// Removal confirms in place rather than in a dialog: one fewer stacking
// context to get wrong, and it keeps the row you are removing on screen.
function genRemoveHtml(row, index) {
    if (genConfirmRemove !== index) {
        return '<button type="button" class="btn btn--danger btn--sm" data-act="remove" data-idx="' + index +
            '" aria-label="Remove ' + escapeHtml(row.name.trim() || 'generation ' + (index + 1)) + '">Remove</button>';
    }
    return (
        '<span class="gen__confirm" role="group" aria-label="Confirm removal">' +
        '<span class="gen__confirm-text">Remove it? Files it already generated stay on disk.</span>' +
        '<button type="button" class="btn btn--danger btn--sm" data-act="remove-yes" data-idx="' + index + '">Remove</button>' +
        '<button type="button" class="btn btn--secondary btn--sm" data-act="remove-no" data-idx="' + index + '">Keep</button>' +
        '</span>'
    );
}

function genNameFieldHtml(row, index, shown) {
    const id = 'gen-name-' + index;
    const bad = !!(shown && shown.field === 'name');
    return (
        '<div class="gen__field gen__field--name">' +
        '<label class="gen__label" for="' + id + '">Name</label>' +
        '<input type="text" id="' + id + '" class="form-input gen__name" data-act="name" data-idx="' + index + '"' +
        ' value="' + escapeHtml(row.name) + '" spellcheck="false" autocomplete="off"' +
        ' aria-describedby="gen-msg-' + index + '"' +
        (bad ? ' aria-invalid="true"' : '') + '>' +
        '</div>'
    );
}

// Under the fields: the way back to the derived name, and the row's message.
// The Reset button is always there and only shown while it has something to
// offer (hidden, it keeps its slot and cannot be reached), so the card is as
// tall with it as without it.
function genNameRowHtml(row, index, shown) {
    const fallback = genDefaultName(row);
    const showReset = !!(row.nameEdited && fallback && row.name.trim() !== fallback);
    return (
        '<div class="gen__namerow">' +
        '<button type="button" class="gen__reset" data-act="reset-name" data-idx="' + index + '"' +
        (showReset ? '' : ' tabindex="-1" aria-hidden="true" disabled') + '>' +
        escapeHtml(fallback ? 'Reset to ' + fallback : 'Reset') + '</button>' +
        '<p class="gen__msg" id="gen-msg-' + index + '"' + (shown ? ' role="alert"' : ' hidden') + '>' +
        escapeHtml(shown ? shown.message : '') + '</p>' +
        '</div>'
    );
}

// The option TEXT is the id, because the id is what the file name is built
// from and what the user thinks in ("hayai-nova-ctd"); a closed select is
// narrow, and a product name like "PaddleOCR-VL 1.6 manga LoRA" only ever
// arrives there clipped. The human label rides along as each option's title
// and, for the selected one, as the field's description: the tooltip on its
// label (marked with an i) and on the select, and the select's accessible
// description -- never a caption under it that makes every card taller.
function genCatalogLabel(list, value) {
    const found = (list || []).find((item) => item.id === value);
    const label = found && found.label ? String(found.label) : '';
    return label && label !== value ? label : '';
}

// A field's label with its description as a tooltip, and the hidden text
// a screen reader reads as the select's description. `desc` is plain text.
function genLabelHtml(id, label, desc) {
    return (
        '<label class="gen__label" for="' + id + '"' + (desc ? ' title="' + escapeHtml(desc) + '"' : '') + '>' +
        escapeHtml(label) + (desc ? '<span class="gen__tip" aria-hidden="true">ⓘ</span>' : '') + '</label>'
    );
}

function genDescHtml(id, desc) {
    return desc ? '<span class="gen__desc sr-only" id="' + id + '-desc">' + escapeHtml(desc) + '</span>' : '';
}

function genDescAttrs(id, desc) {
    return desc ? ' title="' + escapeHtml(desc) + '" aria-describedby="' + id + '-desc"' : '';
}

function genSelectFieldHtml(row, index, act, label, options, value, disabled, hint, bad) {
    const id = 'gen-' + act + '-' + index;
    const opts = options.map((o) =>
        '<option value="' + escapeHtml(o.value) + '"' +
        (o.title && o.title !== o.label ? ' title="' + escapeHtml(o.title) + '"' : '') +
        (String(o.value) === String(value) ? ' selected' : '') + '>' +
        escapeHtml(o.label) + '</option>').join('');
    const missing = value && !options.some((o) => String(o.value) === String(value))
        ? '<option value="' + escapeHtml(value) + '" selected>' + escapeHtml(value) + ' (unknown)</option>'
        : '';
    return (
        '<div class="gen__field">' +
        genLabelHtml(id, label, hint) +
        '<select id="' + id + '" class="form-select gen__select" data-act="' + act + '" data-idx="' + index + '"' +
        genDescAttrs(id, hint) +
        (disabled ? ' disabled' : '') + (bad ? ' aria-invalid="true"' : '') + '>' +
        (value ? '' : '<option value="" selected>Choose…</option>') + missing + opts + '</select>' +
        genDescHtml(id, hint) +
        '</div>'
    );
}

function genDetectorFieldHtml(row, index, locked, effective, shown) {
    const detectorLabel = genCatalogLabel(genCatalog.detectors, effective);
    if (!locked) {
        const hint = detectorLabel
            ? detectorLabel + ' — finds the text regions this engine reads.'
            : 'Finds the text regions this engine reads.';
        return genSelectFieldHtml(row, index, 'detector', 'Detector',
            genCatalog.detectors.map((d) => ({ value: d.id, label: d.id, title: d.label || d.id })),
            row.detector || '', false, hint,
            !!(shown && shown.field === 'detector'));
    }
    const spec = genEngine(row.engine);
    const shownValue = effective || 'built in';
    const owns = spec && (spec.monolithic || spec.served)
        ? row.engine + ' finds the text itself, behind its own command line.'
        : row.engine + ' brings its own detector.';
    const hint = detectorLabel ? detectorLabel + ' — ' + owns : owns;
    const id = 'gen-detector-' + index;
    return (
        '<div class="gen__field">' +
        genLabelHtml(id, 'Detector', hint) +
        '<select id="' + id + '" class="form-select gen__select" data-act="detector" data-idx="' + index + '" disabled' +
        genDescAttrs(id, hint) + '>' +
        '<option selected>' + escapeHtml(shownValue) + '</option></select>' +
        genDescHtml(id, hint) +
        '</div>'
    );
}

// What this generation has done, folded to one line: the library's progress
// and the pages a minute it really runs at (the machines' recent finished
// volumes, never a benchmark). Always there, one line high, whichever machine
// the card shows -- switching machines only changes its words.
function genHistoryHtml(row, index) {
    const parts = genHistoryParts(row);
    return (
        '<details class="gen__history" data-idx="' + index + '"' + (row.historyOpen ? ' open' : '') + '>' +
        '<summary class="gen__history-summary" title="' + escapeHtml(parts.title) + '">' +
        escapeHtml(parts.summary) + '</summary>' +
        '<div class="gen__history-body">' + parts.body + '</div></details>'
    );
}

// {summary, body}: the one line, and what it is made of. In the All view
// (and on a single-machine install, whose one machine is all of them) it is
// the library's: volumes done of the total, pages a minute added up over
// every machine (combined throughput). With one machine chosen it is that
// machine's own contribution: the row's sidecars on disk that it wrote, out
// of the library's total (`volumes_by_machine` -- exact, so the machines sum
// to at most the total), and its own pages a minute. Its lifetime count
// (this server's `local_runs`, a processor's `processor_runs`), which re-runs
// push past the total, is only in the tooltip.
function genHistoryParts(row) {
    const all = genIsAll(row);
    const machines = all ? genAllMachines() : [genMachine(row)];
    const rates = machines.map((machine) => ({ machine: machine, layer: genSpeedLayer(row, machine) }));
    const real = rates.filter((r) => r.layer && typeof r.layer.pages_per_minute === 'number' &&
        r.layer.pages_per_minute > 0);
    const total = real.reduce((sum, r) => sum + r.layer.pages_per_minute, 0);
    const bits = [];
    let who = '';
    let lifetime = '';
    if (!all && genHasMachineChoice(row)) {
        const machine = genMachine(row);
        const runs = machine === 'local' ? row.localRuns : (row.processorRuns || {})[machine];
        const ran = runs && typeof runs.volumes === 'number' && runs.volumes > 0 ? runs.volumes : 0;
        const exact = (row.volumesByMachine || {})[machine];
        const count = typeof exact === 'number' && exact > 0 ? exact : 0;
        if (ran) lifetime = genCount(ran, 'volume') + ' over its lifetime, including re-runs';
        who = genMachineLabel(machine) + ': ';
        // This machine's share of the library's total: "95/129 volumes".
        if (row.volumesTotal !== null) bits.push(count + '/' + row.volumesTotal + ' volumes');
        else if (count) bits.push(genCount(count, 'volume'));
    } else if (row.volumesTotal !== null) {
        bits.push(row.volumesDone + '/' + row.volumesTotal + ' volumes' +
            (row.volumesSkipped > 0 ? ' · ' + row.volumesSkipped + ' skipped' : ''));
    } else if (genStatsPending) {
        bits.push('counting volumes…');
    }
    if (real.length) bits.push(genPpmText(total) + ' pages/min' + (all && real.length > 1 ? ' combined' : ''));
    if (!bits.length) bits.push(row.id ? 'nothing read yet' : 'not saved yet');

    const lines = rates.map((r) => {
        const layer = r.layer;
        const label = '<span class="gen__history-machine">' + escapeHtml(genMachineLabel(r.machine)) + '</span> ';
        if (!layer || !(layer.pages_per_minute > 0)) {
            return '<li data-machine="' + escapeHtml(r.machine) + '">' + label +
                'no volume of this generation finished here yet</li>';
        }
        const ago = agoShort(layer.last_at);
        return (
            '<li data-machine="' + escapeHtml(r.machine) + '"' +
            (layer.last_at ? ' title="' + escapeHtml('last ran ' + processorClock(layer.last_at, true)) + '"' : '') + '>' +
            label + '<strong>' + escapeHtml(genPpmText(layer.pages_per_minute)) + ' pages/min</strong>' +
            (layer.volumes ? ' over its last ' + escapeHtml(genCount(layer.volumes, 'volume')) : '') +
            (ago ? ', last ran ' + escapeHtml(ago) : '') + '</li>'
        );
    });
    if (all && real.length > 1) {
        lines.push('<li class="gen__history-total">Combined: <strong>' + escapeHtml(genPpmText(total)) +
            ' pages/min</strong></li>');
    }
    const summary = 'History — ' + who + bits.join(' · ');
    // The folded line's tooltip carries the machine's lifetime count and why
    // volumes are skipped: the counts are on the line itself, those only here.
    let title = lifetime ? summary + '\n' + lifetime : summary;
    if (row.volumesSkipped > 0) title += '\n\n' + GEN_SKIPPED_WHY;
    return {
        summary: summary,
        title: title,
        body: row.id ? '<ul class="gen__history-machines">' + lines.join('') + '</ul>' : '',
    };
}

// Pages a minute for one line of text: "21", "1,550", "6.2".
function genPpmText(value) {
    return value >= 1000 ? benchThousands(value) : processorPpm(value);
}

// One machine's real throughput of this row, from the Processors card's
// numbers ('local' is this server), or null.
function genSpeedLayer(row, machine) {
    if (!row.id) return null;
    const entry = procSpeed.find((e) => (machine === 'local' ? !!e.local : !e.local && e.name === machine));
    return entry ? (entry.layers || []).find((layer) => layer.generation_id === row.id) || null : null;
}

// The stage table first -- it is what is edited here -- and the congestion
// that argues for changing it folded under it.
function genTuningHtml(row, index) {
    // Pools are per machine: the All view has none to show, and says so in
    // the same one-line slot, so switching machines moves nothing.
    if (genIsAll(row)) {
        return '<p class="gen__tuning gen__tuning--all">Pick a machine to see its pools</p>';
    }
    return (
        '<details class="gen__tuning" data-idx="' + index + '"' + (row.open ? ' open' : '') + '>' +
        '<summary class="gen__tuning-summary">Pools and congestion</summary>' +
        '<div class="gen__tuning-body">' +
        genPoolsHtml(row, index) +
        '<details class="gen__why" data-idx="' + index + '"' + (row.whyOpen ? ' open' : '') + '>' +
        '<summary class="gen__why-summary">Congestion details</summary>' +
        genCongestionHtml(row) +
        '</details>' +
        '</div></details>'
    );
}

function genCongestionHtml(row) {
    const machine = genPoolMachine(row);
    // Per machine (spec section 4): a processor's own runs, never this
    // server's, when the table is for it.
    const congestion = machine === 'local' ? row.congestion : (row.processorCongestion[machine] || null);
    if (machine !== 'local' && !congestion && !row.stagesStale) {
        return '<div class="cong cong--empty"><p>' +
            escapeHtml('No queue runs recorded on ' + genMachineLabel(machine) +
                ' yet. The first volume of this generation it finishes in the queue fills this in.') +
            '</p></div>';
    }
    if (row.stagesStale) {
        return '<div class="cong cong--empty"><p>' + (row.id
            ? 'The engine or detector changed. The numbers here described the old recipe, so they are gone until a volume finishes under this one.'
            : 'This generation has not run yet. Congestion appears once a volume finishes under it.') +
            '</p></div>';
    }
    if (!congestion) {
        const why = genIsMonolithic(row)
            ? 'This engine reads a whole volume in one pass, outside the staged pipeline, so there are no stages to measure and never will be.'
            : 'No queue runs recorded yet. The first volume this generation finishes in the queue fills this in.';
        return '<div class="cong cong--empty"><p>' + why + '</p></div>';
    }
    const stages = congestion.stages || [];
    const queues = congestion.queues || [];
    // What the numbers single out, if anything, heads them: it is said
    // nowhere else on the card.
    return (
        '<div class="cong">' +
        (congestion.verdict
            ? '<p class="cong__verdict">' + escapeHtml(congestion.verdict) + '</p>'
            : '') +
        '<ul class="cong__stages">' +
        stages.map((stage) => genStageRowHtml(stage, stage.key === congestion.bottleneck)).join('') +
        '</ul>' +
        '<p class="cong__legend">' +
        '<span class="cong__key cong__key--busy">busy</span>' +
        '<span class="cong__key cong__key--blocked">blocked by the next stage</span>' +
        '<span class="cong__key cong__key--starved">starved by the one before</span>' +
        '</p>' +
        (queues.length
            ? '<p class="cong__queues-label">Queues between the stages</p>' +
              '<ul class="cong__queues">' + queues.map(genQueueRowHtml).join('') + '</ul>'
            : '') +
        '<p class="cong__meta">Averaged over ' + genCount(congestion.runs, 'run') +
        (congestion.last_run_at ? ', last ' + escapeHtml(genRelativeTime(congestion.last_run_at)) : '') + '.</p>' +
        '</div>'
    );
}

function genStageRowHtml(stage, isBottleneck) {
    const busy = genPct(stage.busy_pct);
    const blocked = genPct(stage.blocked_pct);
    const starved = genPct(stage.starved_pct);
    // Three shares rounded independently upstream can total past 100; the bar
    // is never allowed to be wider than itself.
    const blockedWidth = Math.min(blocked, 100 - busy);
    const starvedWidth = Math.min(starved, 100 - busy - blockedWidth);
    const numbers = 'busy ' + busy + '%, blocked ' + blocked + '%, starved ' + starved + '%';
    return (
        '<li class="cong__stage' + (isBottleneck ? ' cong__stage--worst' : '') + '">' +
        '<span class="cong__stage-name">' + escapeHtml(stage.key) + '</span>' +
        // Where it ran is half of what a width means: "detect · CPU ×3" and
        // "detect · GPU 0 ×1" are different pipelines, not different numbers.
        (stage.device ? '<span class="pools__device pools__device--' +
            (genDeviceIsGpu(stage.device) ? 'gpu' : 'cpu') + '">' +
            escapeHtml(genDeviceShort(stage.device)) + '</span>' : '') +
        '<span class="cong__stage-width">×' + (stage.workers || 1) + '</span>' +
        '<span class="cong__bar" role="img" tabindex="0" aria-label="' + escapeHtml(stage.key) + ': ' + numbers + '"' +
        ' title="' + escapeHtml(stage.key + ' ×' + (stage.workers || 1) + ' — ' + numbers) + '">' +
        '<span class="cong__seg cong__seg--busy" style="width:' + busy + '%"></span>' +
        '<span class="cong__seg cong__seg--blocked" style="width:' + blockedWidth + '%"></span>' +
        '<span class="cong__seg cong__seg--starved" style="width:' + starvedWidth + '%"></span>' +
        '</span>' +
        '<span class="cong__nums">' + numbers + '</span>' +
        '</li>'
    );
}

function genQueueRowHtml(queue) {
    const mean = typeof queue.mean_depth === 'number' ? queue.mean_depth.toFixed(1) : '?';
    return (
        '<li class="cong__queue"><span class="mono">' + escapeHtml(queue.name) + '</span> ' +
        'mean depth ' + escapeHtml(String(mean)) + ' of ' + escapeHtml(String(queue.capacity)) +
        ', peak ' + escapeHtml(String(queue.max_depth)) + '</li>'
    );
}

// The row under the model selects: the Machine select (while there is one)
// and, for an engine that takes one, the row's precision mode beside it with
// one quiet line saying what that mode comes to on the machine shown. The
// line is always there and always one height, so neither switching machines
// nor changing the mode moves anything on the card.
function genMachineRowHtml(row, index) {
    const machine = genHasMachineChoice(row) ? genMachineFieldHtml(row, index) : '';
    const precision = genPrecisionModes(row).length
        ? genPrecisionFieldHtml(row, index) + genPrecisionLineHtml(row, index)
        : '';
    if (!machine && !precision) return '';
    return '<div class="gen__fields gen__fields--machine">' + machine + precision + '</div>';
}

// The card's Machine select, under the model selects: All machines (the
// default), this server, or one connected processor. It decides whose numbers
// the card shows and whose pools it edits. Only while there is more than one
// machine, and only on a saved row (a processor's pools are filed under the
// row's id).
function genMachineFieldHtml(row, index) {
    if (!genHasMachineChoice(row)) return '';
    const chosen = genMachine(row);
    const options = [{ name: GEN_ALL, label: 'All machines' }, { name: 'local', label: 'this server' }].concat(
        genProcessors.map((p) => ({ name: p.name, label: p.label || p.name }))
    );
    const id = 'gen-machine-' + index;
    const desc = 'Whose numbers this card shows and whose pools it edits. All machines adds every ' +
        'machine\'s numbers up, and benchmarks every machine that can run this generation.';
    return (
        '<div class="gen__field gen__field--machine">' +
        genLabelHtml(id, 'Machine', desc) +
        '<select id="' + id + '" class="form-select gen__select pools__processor" data-act="machine"' +
        ' data-idx="' + index + '" data-gen="' + escapeHtml(row.id || '') + '"' + genDescAttrs(id, desc) + '>' +
        options.map((o) => '<option value="' + escapeHtml(o.name) + '"' +
            (o.name === chosen ? ' selected' : '') + '>' + escapeHtml(o.label) + '</option>').join('') +
        '</select>' + genDescHtml(id, desc) + '</div>'
    );
}

// The rules, in plain words: the precision label's tooltip, the hint's, and
// the select's accessible description.
const GEN_PRECISION_RULES =
    'One mode for this generation, on every machine. ' +
    'Auto: accuracy (the default) picks what tested most accurate for each engine, and never fp16. ' +
    'Auto: balanced gives up a little accuracy for a lot of speed; Auto: speed takes the fastest format ' +
    'each card runs well. For those two, each machine benchmarks this generation automatically before its ' +
    'first volume, and again when the mode changes, and keeps the fastest (within 5%, the more accurate ' +
    'one). Only with automatic benchmarks off, or when a machine\'s benchmark failed, does it use the ' +
    'first format its card supports. fp32, bf16 or fp16 only: a machine whose card cannot run that format ' +
    'is not eligible. mokuro never runs bf16.';

function genPrecisionFieldHtml(row, index) {
    const fallback = genDefaultMode();
    const options = genPrecisionModes(row).map((mode) => ({
        value: mode,
        label: genModeLabel(mode) + (mode === fallback ? ' (default)' : ''),
    }));
    return (
        genSelectFieldHtml(row, index, 'precision', 'Precision', options, row.precision,
            false, GEN_PRECISION_RULES, false).replace('class="gen__field"', 'class="gen__field gen__field--precision"') +
        '<p class="gen__precision-hint" title="' + escapeHtml(GEN_PRECISION_RULES) + '">' +
        'One mode for every machine.</p>'
    );
}

// The line under the mode: what it comes to on the machine the card shows,
// and -- whichever machine that is -- the plain reason when nobody connected
// can run it. Empty in All machines while nothing is held, one height always.
function genPrecisionLineHtml(row, index) {
    const machine = genMachine(row);
    const resolution = machine === GEN_ALL ? '' : genResolutionText(row, machine);
    const hold = genPrecisionHold(row);
    const entry = machine === GEN_ALL ? null : genModeOn(row, machine);
    const why = entry && entry.eligible !== false && entry.why ? ' (' + entry.why + ')' : '';
    const said = [resolution ? resolution + why : '', hold || ''].filter(Boolean).join(' · ');
    const parts = [];
    if (resolution) parts.push('<span class="gen__precision-here">' + escapeHtml(resolution) + '</span>');
    if (hold) parts.push('<span class="gen__precision-hold" role="status">' + escapeHtml(hold) + '</span>');
    return (
        '<p class="gen__precision-line" id="gen-precision-line-' + index + '" data-idx="' + index + '"' +
        (said ? ' title="' + escapeHtml(said) + '"' : '') + '>' +
        parts.join('<span class="gen__precision-sep" aria-hidden="true"> · </span>') + '</p>'
    );
}

// Did somebody set anything in this table? A width, a capacity or a device
// (auto spelled out too): then this server runs it as written. Left empty,
// the server benchmarks the row on its own hardware and runs what that finds
// -- kept in its own profile, never in the config.
function genPoolsConfigured(pools) {
    if (!pools) return false;
    return ['stage_workers', 'queue_capacity', 'stage_device'].some(
        (key) => Object.keys(pools[key] || {}).length > 0);
}

// "detect ×3, engine on gpu:0" -- what a stored pools object says.
function genPoolsSummary(pools) {
    const bits = [];
    Object.keys(pools.stage_workers || {}).forEach((key) => {
        const value = pools.stage_workers[key];
        bits.push(key + (value === 'auto' ? ' derived' : ' ×' + value));
    });
    Object.keys(pools.queue_capacity || {}).forEach((key) => {
        bits.push(key + ' queue ' + pools.queue_capacity[key]);
    });
    Object.keys(pools.stage_device || {}).forEach((key) => {
        bits.push(key + ' on ' + pools.stage_device[key]);
    });
    return bits.join(', ');
}

// This server's table: a row nobody configured runs its own benchmark's pools.
function genLocalNoteHtml(row) {
    if (!row.id || !genLocalProcessing || genPoolsConfigured(row.pools)) return '';
    if (!row.localPools && !row.localBench && !genAutobench) return '';
    const bits = [];
    if (row.localPools) {
        bits.push('Not configured by hand: this server runs what its benchmark found (' +
            genPoolsSummary(row.localPools) + ').');
    } else if (row.localBench) {
        bits.push('Not configured by hand: this server\'s benchmark found the derived sizes best.');
    } else {
        bits.push('Not configured by hand: this server benchmarks it before it runs it.');
    }
    const pps = row.localBench && row.localBench.pages_per_second;
    if (typeof pps === 'number') bits.push('Benchmark: ' + pps.toFixed(1) + ' pages a second.');
    bits.push('Set any value to configure it yourself.');
    return '<p class="pools__machine-note pools__machine-note--local">' +
        escapeHtml(bits.join(' ')) + '</p>';
}

// What one processor's evidence says about this row, beside its name.
function genMachineNoteHtml(row, name) {
    if (name === 'local') return genLocalNoteHtml(row);
    const stored = row.processorPools[name];
    const runs = row.processorRuns[name];
    const bits = [];
    if (genHoldsPools(stored)) {
        bits.push('Saved for ' + genMachineLabel(name) + '.');
    } else if (genBenchChangedNothing(row, name)) {
        // A benchmark whose best is the table it measured stores no pools:
        // "no entry yet" would read as though nothing had been done there.
        bits.push(name + ' runs this row\'s own table (its benchmark found nothing to change).');
    } else {
        bits.push('No entry for ' + genMachineLabel(name) + ' yet: it runs this row\'s own table.');
    }
    if (runs && runs.volumes) {
        bits.push(genCount(runs.volumes, 'volume') + ' read there, ' +
            (typeof runs.pages_per_second === 'number' ? runs.pages_per_second.toFixed(1) : '?') +
            ' pages a second on average.');
    }
    return '<p class="pools__machine-note">' + escapeHtml(bits.join(' ')) + '</p>';
}

// Did this machine's last benchmark of the row find nothing better than the
// table it measured ("best: auto")? Only a result on hand says so.
function genBenchChangedNothing(row, name) {
    const result = benchResultFor(benchKeyFor(row), name);
    if (!result || result.tunable === false || !result.best) return false;
    if (benchShortWindow(result.best)) return false;
    return result.best.same_as_spec === true || !benchHasPools(result.best);
}

// What an empty box means, on the column headers it applies to (the greyed
// placeholder in the box says the rest).
const GEN_POOLS_BLANK_TIP = 'Leave a box empty to let the server size it; the greyed number is what it picked. ' +
    'Widen the stage a queue is backing up behind.';
// Where to place the stages, on the Device header: the short version of
// docs/configuration.md's pools section, in plain words.
const GEN_POOLS_DEVICE_TIP = 'Run detection on the CPU to leave the whole card to the engine; ' +
    'a CPU engine is for machines without a card.';

function genPoolsHtml(row, index) {
    const machine = genPoolMachine(row);
    // The Machine select is on the card now, above everything it switches.
    const selector = '';
    if (row.stagesStale) {
        return '<div class="pools pools--empty">' + selector + '<p>Working out this generation’s stages…</p></div>';
    }
    const stages = genActiveStages(row);
    if (stages === null) {
        return '<div class="pools pools--empty">' + selector +
            '<p>Working out this generation’s stages on ' + escapeHtml(genMachineLabel(machine)) + '…</p></div>';
    }
    if (!stages.length) {
        return '<div class="pools pools--empty">' + selector + '<p>The server has not reported stages for this generation yet.</p></div>';
    }
    const rows = stages.map((stage) => genPoolRowHtml(row, index, stage)).join('');
    const saveForMachine = machine === 'local' ? '' :
        '<div class="gen-actions"><button type="button" class="btn btn--primary btn--sm" data-act="pools-save"' +
        ' data-idx="' + index + '">Save for ' + escapeHtml(genMachineLabel(machine)) + '</button></div>';
    return (
        '<div class="pools">' +
        selector +
        genMachineNoteHtml(row, machine) +
        '<div class="table-container"><table class="pools__table">' +
        '<thead><tr><th scope="col">Stage</th>' +
        '<th scope="col"><span class="pools__th-tip" title="' + escapeHtml(GEN_POOLS_DEVICE_TIP) + '">Device</span></th>' +
        '<th scope="col" class="num"><span class="pools__th-tip" title="' + escapeHtml(GEN_POOLS_BLANK_TIP) + '">Workers</span></th>' +
        '<th scope="col" class="num"><span class="pools__th-tip" title="' + escapeHtml(GEN_POOLS_BLANK_TIP) + '">Queue capacity</span></th></tr></thead>' +
        '<tbody>' + rows + '</tbody></table></div>' +
        saveForMachine +
        '</div>'
    );
}

// A device id is "auto", "cpu" or "gpu:<n>" (Addendum 7). The card's INDEX is
// part of it: with two rows pinned to two cards, "GPU 1" is the point.
function genDeviceIsGpu(device) {
    return String(device || '').indexOf('gpu') === 0;
}

function genDeviceLabel(device) {
    const text = String(device || 'cpu');
    const known = (genCatalog.devices || []).find((entry) => entry.id === text);
    if (known && known.label) return known.label;
    if (text === 'cpu') return 'CPU';
    if (text === 'auto') return 'Auto';
    return genDeviceIsGpu(text) ? 'GPU ' + text.slice(4) : text;
}

function genDeviceShort(device) {
    const text = String(device || 'cpu');
    if (text === 'cpu') return 'CPU';
    // "gpu" with no index is what a run recorded before devices were chosen
    // per stage; it means a card without saying which.
    if (text === 'gpu') return 'GPU';
    return genDeviceIsGpu(text) ? 'GPU ' + text.slice(4) : text;
}

function genDeviceCellHtml(row, index, stage) {
    const allowed = stage.devices_allowed || [];
    const chosen = genActivePools(row).stage_device[stage.key] || 'auto';
    // A stage with no model is not a choice: post and layout are assembly and
    // JSON, and the cell says so instead of offering a control that lies.
    if (!allowed.length) {
        return '<td><span class="pools__device pools__device--cpu">' +
            escapeHtml(genDeviceShort(stage.device)) + '</span></td>';
    }
    if (stage.device_locked_reason) {
        return '<td><span class="pools__device pools__device--cpu">CPU</span>' +
            '<span class="pools__why">' + escapeHtml(stage.device_locked_reason) + '</span></td>';
    }
    // The labels are the machine's OWN (`device_options`, from the catalog of
    // the machine this table is for): the page-wide catalog is this server's,
    // and a processor's select must never offer this server's hardware.
    const named = {};
    (stage.device_options || []).forEach((entry) => {
        if (entry && entry.id) named[entry.id] = entry.label;
    });
    const options = allowed.map((id) => {
        // "auto" shows what it RESOLVED to, so the row never hides which card
        // it is actually on.
        const label = named[id] ||
            (id === 'auto' ? 'Auto → ' + genDeviceShort(stage.device) : genDeviceLabel(id));
        return '<option value="' + escapeHtml(id) + '"' + (id === chosen ? ' selected' : '') + '>' +
            escapeHtml(label) + '</option>';
    }).join('');
    return (
        '<td><select class="form-input pools__device-select" data-act="device" data-idx="' + index +
        '" data-stage="' + escapeHtml(stage.key) + '"' +
        ' aria-label="' + escapeHtml(stage.key) + ' device">' + options + '</select></td>'
    );
}

// The runner's MAX_ENGINE_COPIES: each copy is a process holding the whole
// model on the card, so the box stops where a copy stops paying for itself.
const GEN_MAX_ENGINE_COPIES = 8;

function genPoolRowHtml(row, index, stage) {
    const active = genActivePools(row);
    const workers = active.stage_workers[stage.key];
    const capacity = active.queue_capacity[stage.key];
    // The ENGINE's own pipeline width, not a pool of ours: the fork's
    // --num_workers, whether it is reached through the serve process (the
    // served road's `mokuro` stage) or through the one-volume command line (a
    // monolithic row's single stage). So the box stays live although the stage
    // is device-bound, and there is no derived number of ours to suggest in it.
    const engineWorkers = stage.workers_means === 'engine';
    // A recognizer's engine stage on a card: the number is how many copies
    // of the model the session runs there, each in a process of its own.
    // Blank is one copy, the way the stage has always run.
    const copies = stage.workers_means === 'copies';
    // A monolithic row's one stage has no queue between stages either.
    const forkStage = stage.derived_workers == null && stage.derived_capacity == null;
    // One model on one device: a stage holding a model on a card runs one
    // copy of it, so its width is not a box to type in.
    const fixed = !engineWorkers && !copies && (stage.max_workers === 1 || genDeviceIsGpu(stage.device));
    const workersCell = fixed
        ? '<td class="num pools__fixed">1 <span class="pools__why">one model on one device</span></td>'
        : copies
        ? '<td class="num"><input type="number" class="form-input pools__input" data-act="workers" data-idx="' + index +
          '" data-stage="' + escapeHtml(stage.key) + '" min="1" max="' + GEN_MAX_ENGINE_COPIES + '" placeholder="1"' +
          ' value="' + escapeHtml(typeof workers === 'number' ? String(workers) : '') + '"' +
          ' aria-label="' + escapeHtml(stage.key) + ' copies of the model on the card, blank for one">' +
          ' <span class="pools__why">copies of the model on the card</span></td>'
        : '<td class="num"><input type="number" class="form-input pools__input" data-act="workers" data-idx="' + index +
          '" data-stage="' + escapeHtml(stage.key) + '" min="1"' +
          // Uncapped for the engine's own pool: our ceiling of 1 is
          // structural (one process, one model) and says nothing about how
          // wide the fork's pipeline inside it may be.
          (stage.max_workers && !engineWorkers ? ' max="' + escapeHtml(String(stage.max_workers)) + '"' : '') +
          // An engine's own pipeline width is not derived by us: left blank,
          // the engine picks it, so showing OUR derived 1 there would be a
          // number nothing will use.
          ' placeholder="' + escapeHtml(engineWorkers || stage.derived_workers == null
              ? 'auto' : String(stage.derived_workers)) + '"' +
          // "auto (fork default)" did not fit the cell: the fork part is said
          // on hover instead.
          (engineWorkers ? ' title="Blank: the engine\'s own default width"' : '') +
          // A machine's stored `auto` is derived there: the blank cell.
          ' value="' + escapeHtml(typeof workers === 'number' ? String(workers) : '') + '"' +
          ' aria-label="' + escapeHtml(stage.key) + ' workers, blank for automatic"></td>';
    const capacityCell = forkStage
        ? '<td class="num pools__fixed">—</td>'
        : '<td class="num"><input type="number" class="form-input pools__input" data-act="capacity" data-idx="' + index +
          '" data-stage="' + escapeHtml(stage.key) + '" min="1"' +
          ' placeholder="' + escapeHtml(stage.derived_capacity == null ? 'auto' : String(stage.derived_capacity)) + '"' +
          ' value="' + escapeHtml(typeof capacity === 'number' ? String(capacity) : '') + '"' +
          ' aria-label="' + escapeHtml(stage.key) + ' queue capacity, blank for automatic"></td>';
    return (
        '<tr><th scope="row"><span class="mono">' + escapeHtml(stage.key) + '</span>' +
        (stage.name && stage.name !== stage.key ? '<span class="pools__stage-name">' + escapeHtml(stage.name) + '</span>' : '') +
        '</th>' +
        genDeviceCellHtml(row, index, stage) +
        workersCell +
        capacityCell + '</tr>'
    );
}

function genPct(value) {
    if (typeof value !== 'number' || !isFinite(value) || value < 0) return 0;
    return Math.min(100, Math.round(value));
}

function genCount(n, noun) {
    const count = typeof n === 'number' ? n : 0;
    return count + ' ' + noun + (count === 1 ? '' : 's');
}

function genRelativeTime(iso) {
    const then = Date.parse(iso);
    if (isNaN(then)) return String(iso);
    const seconds = Math.max(0, Math.round((Date.now() - then) / 1000));
    if (seconds < 90) return 'just now';
    const minutes = Math.round(seconds / 60);
    if (minutes < 90) return minutes + ' minutes ago';
    const hours = Math.round(minutes / 60);
    if (hours < 36) return hours + ' hours ago';
    return Math.round(hours / 24) + ' days ago';
}

// ---- edits --------------------------------------------------------------

function genRowAt(target) {
    const index = parseInt(target.dataset.idx, 10);
    return { index: index, row: genRows[index] };
}

function genTouched(index) {
    if (genServerError && genServerError.row === index) genServerError = null;
}

function onGenInput(event) {
    const target = event.target;
    const act = target.dataset.act;
    if (!act) return;
    const spot = genRowAt(target);
    if (!spot.row) return;
    if (act === 'name') {
        // Typing never re-renders: that would take the caret with it. Only the
        // Reset offer, the messages and the Save state are refreshed.
        spot.row.name = target.value;
        spot.row.nameEdited = true;
        genTouched(spot.index);
        genRefreshValidation();
    } else if (act === 'workers' || act === 'capacity') {
        const active = genActivePools(spot.row);
        const pool = act === 'workers' ? active.stage_workers : active.queue_capacity;
        const key = target.dataset.stage;
        const raw = target.value.trim();
        if (raw === '') delete pool[key];
        else pool[key] = parseInt(raw, 10);
        genTouched(spot.index);
        genRefreshValidation();
    }
}

function onGenChange(event) {
    const target = event.target;
    const act = target.dataset.act;
    if (!act) return;
    const spot = genRowAt(target);
    if (!spot.row) return;
    genTouched(spot.index);
    if (act === 'engine') {
        spot.row.engine = target.value;
        genRecipeChanged(spot.row);
        renderGenerations('[data-act="engine"][data-idx="' + spot.index + '"]');
        genDeriveStages(spot.row);
    } else if (act === 'detector') {
        spot.row.detector = target.value || null;
        genRecipeChanged(spot.row);
        renderGenerations('[data-act="detector"][data-idx="' + spot.index + '"]');
        genDeriveStages(spot.row);
    } else if (act === 'machine') {
        // Switching what the card shows must not unfold anything: the result
        // keeps the fold it has now instead of opening itself because the
        // new machine's result has settings to apply.
        const key = benchKeyFor(spot.row);
        if (!Object.prototype.hasOwnProperty.call(genBenchOpen, key)) {
            const shownResult = document.querySelector('.gen[data-idx="' + spot.index + '"] details.bench-res');
            genBenchOpen[key] = !!(shownResult && shownResult.open);
        }
        spot.row.machine = target.value || GEN_ALL;
        genSyncFolds(spot.index);
        renderGenerations('[data-act="machine"][data-idx="' + spot.index + '"]');
        if (genPoolMachine(spot.row) !== 'local' && genActiveStages(spot.row) === null) {
            genDeriveStages(spot.row);
        }
        // A benchmark runs on the machine chosen here (spec section 5), and
        // the one shown is that machine's (in the All view, every machine's).
        benchProbeRow(spot.row);
        benchRefreshAll();
    } else if (act === 'device') {
        const key = target.dataset.stage;
        const devices = genActivePools(spot.row).stage_device;
        if (target.value === 'auto') delete devices[key];
        else devices[key] = target.value;
        // A stage that moved between the CPU and a card changes what its
        // Workers cell may be, so the table is re-derived rather than guessed
        // at here: the server owns that rule.
        genRefreshValidation();
        genDeriveStages(spot.row);
    } else if (act === 'precision') {
        // One mode for every machine: only the line under it changes, in
        // place, and whatever reads the mode (Save, Benchmark all) follows.
        spot.row.precision = target.value || genDefaultMode();
        const line = document.getElementById('gen-precision-line-' + spot.index);
        if (line) line.outerHTML = genPrecisionLineHtml(spot.row, spot.index);
        genRefreshValidation();
    } else if (act === 'patch') {
        spot.row.patch_budget = target.value === '' ? null : parseInt(target.value, 10);
        genRefreshValidation();
    } else if (act === 'enabled') {
        spot.row.enabled = target.checked;
        if (!spot.row.enabled && spot.row.primary) genMovePrimaryOff(spot.row);
        renderGenerations('[data-act="enabled"][data-idx="' + spot.index + '"]');
    } else if (act === 'primary') {
        genRows.forEach((other) => { other.primary = other === spot.row; });
        renderGenerations('[data-act="primary"][data-idx="' + spot.index + '"]');
    }
}

// Disabling the primary row would leave the list with no bare .mokuro, so the
// flag moves to the first row still enabled and says where it went.
function genMovePrimaryOff(row) {
    const heir = genRows.find((other) => other !== row && other.enabled);
    if (!heir) return;
    row.primary = false;
    heir.primary = true;
    showToast('Primary moved to ' + (heir.name.trim() || 'generation ' + (genRows.indexOf(heir) + 1)), 'info');
}

// The stage keys belong to the server: after an engine or detector change we
// do not know them until it answers, so the overrides go rather than being
// re-pinned onto keys that may not exist.
function genRecipeChanged(row) {
    row.stagesStale = true;
    row.stages = [];
    // Every machine's table described the old recipe.
    row.machineStages = {};
    row.pools = { stage_workers: {}, queue_capacity: {}, stage_device: {} };
    // A mode the new engine does not offer goes back to the default.
    if (genPrecisionModes(row).indexOf(row.precision) === -1) row.precision = genDefaultMode();
    if (genDetectorLocked(row)) row.detector = null;
    if (!row.nameEdited) row.name = genDefaultName(row);
    if (!genUsesPatchBudget(row)) row.patch_budget = null;
    else if (row.patch_budget === null && genCatalog.patch_budgets.length) {
        row.patch_budget = genCatalog.patch_budgets[genCatalog.patch_budgets.length - 1];
    }
    row.open = true;
}

// The stages, their devices and their derived widths are the SERVER's answer
// for the row as it is edited right now -- engine, detector and the devices
// chosen -- so the table follows an edit without a save and without this file
// duplicating the runner's rules. A refusal or an unreachable server leaves
// the row's last stages alone rather than blanking the table.
async function genDeriveStages(row) {
    const attempt = ++row.deriveSeq;
    const machine = genPoolMachine(row);
    try {
        const request = { spec: genSpecFor(row, machine) };
        // For a processor, the stages THAT machine would run: its cards in
        // the Device select, its cores in the derived widths.
        if (machine !== 'local') request.processor = machine;
        const body = await apiPost('/ocr/generations/derive', request);
        if (attempt !== row.deriveSeq || genRows.indexOf(row) === -1) return;
        const stages = Array.isArray(body.stages) ? body.stages : [];
        if (machine === 'local') {
            row.stages = stages;
            row.road = body.road || null;
        } else {
            row.machineStages[machine] = stages;
        }
        row.stagesStale = false;
        // Pool entries naming a stage this row no longer has would be refused
        // on save, so they go with the stages they named.
        const keys = stages.map((stage) => stage.key);
        const pools = machine === 'local' ? row.pools : genActivePools(row);
        ['stage_workers', 'queue_capacity', 'stage_device'].forEach((pool) => {
            Object.keys(pools[pool]).forEach((key) => {
                if (keys.indexOf(key) === -1) delete pools[pool][key];
            });
        });
    } catch (err) {
        if (attempt !== row.deriveSeq) return;
        row.stagesStale = false;
    }
    // The answer arrives while someone is typing somewhere on this page, so
    // the re-render puts the caret back where it was.
    renderGenerations(genFocusSelector());
}

// Where the focus is right now, as a selector this list can find again.
function genFocusSelector() {
    const el = document.activeElement;
    if (!el || !el.dataset || !el.dataset.act) return null;
    const list = document.getElementById('gen-list');
    if (!list || !list.contains(el)) return null;
    let selector = '[data-act="' + el.dataset.act + '"]';
    if (el.dataset.idx != null) selector += '[data-idx="' + el.dataset.idx + '"]';
    if (el.dataset.stage) selector += '[data-stage="' + el.dataset.stage + '"]';
    return selector;
}

function onGenClick(event) {
    const target = event.target.closest('[data-act]');
    if (!target || !document.getElementById('gen-list').contains(target)) return;
    const act = target.dataset.act;
    const known = ['up', 'down', 'remove', 'remove-yes', 'remove-no', 'reset-name',
                   'bench-start', 'bench-cancel', 'bench-apply', 'pools-save'];
    if (known.indexOf(act) === -1) return;
    const index = parseInt(target.dataset.idx, 10);
    const row = genRows[index];
    if (!row) return;
    if (act.indexOf('bench-') === 0) {
        if (act === 'bench-start') startBench(index);
        else if (act === 'bench-cancel') cancelBench(index);
        else if (act === 'bench-apply') applyBench(index);
        return;
    }
    if (act === 'pools-save') {
        saveMachinePools(index);
        return;
    }
    genTouched(index);
    if (act === 'up' || act === 'down') {
        const to = act === 'up' ? index - 1 : index + 1;
        if (to < 0 || to >= genRows.length) return;
        genRows.splice(to, 0, genRows.splice(index, 1)[0]);
        genConfirmRemove = -1;
        renderGenerations('[data-act="' + act + '"][data-idx="' + to + '"]');
    } else if (act === 'remove') {
        genConfirmRemove = index;
        renderGenerations('[data-act="remove-yes"][data-idx="' + index + '"]');
    } else if (act === 'remove-no') {
        genConfirmRemove = -1;
        renderGenerations('[data-act="remove"][data-idx="' + index + '"]');
    } else if (act === 'remove-yes') {
        benchForget(benchKeyFor(row));
        genRows.splice(index, 1);
        genConfirmRemove = -1;
        if (!genRows.some((other) => other.primary && other.enabled)) {
            const heir = genRows.find((other) => other.enabled);
            if (heir) heir.primary = true;
        }
        renderGenerations();
        document.getElementById('gen-add-btn').focus();
    } else if (act === 'reset-name') {
        row.nameEdited = false;
        row.name = genDefaultName(row);
        renderGenerations('[data-act="name"][data-idx="' + index + '"]');
    }
}

function addGeneration() {
    // A new row defaults to an engine of the ENGINES environment: a composed
    // one, whose detector is a choice. The mokuro engines live in their own
    // environment and find their own text, so they are a deliberate pick.
    const engine = genCatalog.engines.find((e) => !e.monolithic && !e.own_environment)
        || genCatalog.engines[0];
    const detector = genOwnsDetection(engine) ? null : (genCatalog.detectors[0] || null);
    const row = {
        id: null,
        key: 'g' + (++genSeq),
        name: '',
        primary: !genRows.some((other) => other.primary && other.enabled),
        enabled: true,
        engine: engine ? engine.id : '',
        detector: detector ? detector.id : null,
        patch_budget: null,
        precision: genDefaultMode(),
        pools: { stage_workers: {}, queue_capacity: {}, stage_device: {} },
        sidecar: null,
        effectiveDetector: null,
        detectorLocked: false,
        patchBudgetApplies: false,
        precisionOn: null,
        precisionHold: null,
        savedEngine: null,
        savedPrecision: null,
        road: null,
        stages: [],
        volumesDone: null,
        volumesTotal: null,
        volumesSkipped: 0,
        volumesByMachine: {},
        congestion: null,
        nameEdited: false,
        open: false,
        stagesStale: true,
        deriveSeq: 0,
    };
    if (engine && engine.patch_budget && genCatalog.patch_budgets.length) {
        row.patch_budget = genCatalog.patch_budgets[genCatalog.patch_budgets.length - 1];
    }
    genRows.push(row);
    row.name = genDefaultName(row);
    genConfirmRemove = -1;
    renderGenerations('[data-act="name"][data-idx="' + (genRows.length - 1) + '"]');
    // A brand-new row has no stages until the server names them, and it can
    // say so before the row is ever saved.
    genDeriveStages(row);
}

// Everything that can change without the list's shape changing: the Reset
// offer, the per-row message, and whether Save is allowed.
function genRefreshValidation() {
    const list = document.getElementById('gen-list');
    if (!list) return;
    genRows.forEach((row, index) => {
        const item = list.querySelector('.gen[data-idx="' + index + '"]');
        if (!item) return;
        const error = (genServerError && genServerError.row === index) ? genServerError : genRowError(row);
        // The message and the reset affordance follow the name, and neither
        // can hold the caret.
        const namerow = item.querySelector('.gen__namerow');
        if (namerow) namerow.outerHTML = genNameRowHtml(row, index, error);
        item.classList.toggle('gen--invalid', !!error);
        const input = item.querySelector('.gen__name');
        if (input) {
            if (error && error.field === 'name') input.setAttribute('aria-invalid', 'true');
            else input.removeAttribute('aria-invalid');
        }
    });
    const banner = document.getElementById('gen-banner');
    const listError = genListError();
    banner.hidden = !listError;
    banner.textContent = listError || '';
    const dirty = genIsDirty();
    document.getElementById('gen-save-btn').disabled = genRows.some((row) => genRowError(row)) || !!listError;
    document.getElementById('gen-revert-btn').disabled = !dirty;
    document.getElementById('gen-dirty').hidden = !dirty;
    // A benchmark measures the row exactly as shown, so an edit to anything
    // that feeds `genRowSpec` (name aside) can flip whether a past result is
    // stale or whether Apply has anything left to offer -- keep both live.
    benchRefreshAll();
}

// ---- saving -------------------------------------------------------------

// The stored fields only, in list order. `id` is left out of a new row for the
// server to mint; `detector` is null where the engine brings its own.
function genPutRows() {
    return genRows.map((row) => {
        const body = Object.assign({}, genRowSpec(row));
        if (row.id) body.id = row.id;
        body.name = row.name.trim();
        body.primary = !!row.primary;
        body.enabled = !!row.enabled;
        return body;
    });
}

// engine/detector/patch_budget/precision/pools, exactly as the row is shown
// right now -- unsaved edits included. This is the PUT body's row shape minus
// id/name/primary/enabled, and it is ALSO what a benchmark measures
// (ADDENDUM 5: "spec is the row AS CURRENTLY EDITED"), so the two are built
// by the one function rather than kept in sync by hand.
function genRowSpec(row) {
    const spec = {
        engine: row.engine,
        detector: genDetectorLocked(row) ? null : (row.detector || null),
        patch_budget: row.patch_budget === null ? null : row.patch_budget,
    };
    // The row's one mode, always said for an engine that takes one; never
    // in the pools (they are per machine, and the mode is not).
    if (genPrecisionModes(row).length) spec.precision = row.precision || genDefaultMode();
    spec.pools = {
        stage_workers: genCleanPool(row.pools.stage_workers),
        queue_capacity: genCleanPool(row.pools.queue_capacity),
        stage_device: genCleanDevices(row.pools.stage_device),
    };
    return spec;
}

// Key order is not part of "the same spec": a spec just echoed back by the
// server must still compare equal to one built fresh from the row.
function genStableJson(value) {
    if (value === null || typeof value !== 'object') return JSON.stringify(value);
    if (Array.isArray(value)) return '[' + value.map(genStableJson).join(',') + ']';
    return '{' + Object.keys(value).sort()
        .map((k) => JSON.stringify(k) + ':' + genStableJson(value[k])).join(',') + '}';
}

function genSpecsEqual(a, b) {
    return genStableJson(a || {}) === genStableJson(b || {});
}

// "auto" is the ABSENCE of a choice, so it is not sent: a stored "auto" and
// a missing key would then mean the same thing two ways.
function genCleanDevices(pool) {
    const out = {};
    Object.keys(pool || {}).forEach((key) => {
        const value = pool[key];
        if (typeof value === 'string' && value && value !== 'auto') out[key] = value;
    });
    return out;
}

function genCleanPool(pool) {
    const out = {};
    Object.keys(pool).forEach((key) => {
        const value = pool[key];
        if (typeof value === 'number' && isFinite(value) && value > 0) out[key] = value;
    });
    return out;
}

function genIsDirty() {
    if (genSavedJson === null) return false;
    return JSON.stringify(genPutRows()) !== genSavedJson;
}

async function savePoolsFor(generationId, processorName, pools) {
    if (processorName === 'local') return saveGenerations();
    return apiPut('/ocr/generations/' + encodeURIComponent(generationId) + '/pools', {
        processor: processorName,
        pools: pools,
    });
}

// One processor's pools for one row: never part of the list's own Save, and
// never in the config -- they live in that machine's profile and take effect
// on its next session.
async function saveMachinePools(index) {
    const row = genRows[index];
    if (!row || !row.id) return;
    const machine = genPoolMachine(row);
    if (machine === 'local') return;
    const pools = genActivePools(row);
    // `auto` is said, not left out, where the row's own table pins the stage:
    // a table of a machine's pools that names nothing is no opinion, and the
    // row's pin is what would run there -- for a width or a capacity exactly
    // as for a device.
    const said = (clean, table) => {
        const out = clean(pools[table]);
        Object.keys(clean(row.pools[table] || {})).forEach((key) => {
            if (!(key in out)) out[key] = 'auto';
        });
        return out;
    };
    try {
        const body = {
            stage_workers: said(genCleanPool, 'stage_workers'),
            queue_capacity: said(genCleanPool, 'queue_capacity'),
            stage_device: said(genCleanDevices, 'stage_device'),
        };
        const saved = await savePoolsFor(row.id, machine, body);
        row.processorPools[machine] = saved.pools || {};
        row.machinePools[machine] = genClonePools(saved.pools);
        showToast('Pools saved for ' + genMachineLabel(machine) + '; its next session uses them', 'success');
        renderGenerations();
    } catch (err) {
        showToast(err.message, 'error');
    }
}

function revertGenerations() {
    genServerError = null;
    loadGenerations();
}

async function saveGenerations() {
    const note = document.getElementById('gen-save-note');
    try {
        const result = await apiPut('/ocr/generations', { generations: genPutRows() });
        adoptGenerations(result);
        const message = liveApplyMessage(result, 'Generations saved');
        note.textContent = message + '.';
        note.hidden = false;
        showToast(message, result?.restart_required ? 'warning' : 'success');
    } catch (err) {
        const payload = err.payload || {};
        note.hidden = true;
        if (typeof payload.row === 'number' && genRows[payload.row]) {
            genServerError = { row: payload.row, field: payload.field || null, message: err.message };
            renderGenerations();
            const item = document.querySelector('.gen[data-idx="' + payload.row + '"]');
            if (item) {
                item.scrollIntoView({ behavior: 'smooth', block: 'center' });
                const field = item.querySelector('[data-act="' + (payload.field || 'name') + '"]');
                if (field && !field.disabled) field.focus();
            }
        }
        showToast(err.message, 'error');
    }
}

// ============================================
// Benchmark & tune
// Every speed this page claims for a generation is measured HERE, on this
// machine, on pages out of this library -- the table at the bottom of the
// section is the developers' hardware and cannot answer for anyone else's.
//
// ADDENDA 5 & 6: a benchmark measures a SPEC -- the row exactly as shown on
// screen, saved or not -- under a KEY: the generation's id once it has one,
// else a draft id this page mints and keeps for the row's life here. Several
// keys can be queued at once and run strictly in the order they were posted;
// posting is enabled for every row all the time, and the ONLY thing that
// disables a row's own button is that row's OWN benchmark already being
// queued or running (it shows its place in line and a Cancel instead). The
// OCR queue is held and pre-empted once, for the whole line, and released
// once the line is empty again.
// ============================================

const BENCH_POLL_MS = 2000;
const BENCH_VOLUME_PAGES = 200;
// ADDENDUM 9. A rate read over a window shorter than this was not read over
// enough of the machine to mean anything, and the server never decided
// anything on it; neither may this page show it as if it had.
const BENCH_SHORT_WINDOW_SECONDS = 10;

// Keyed by BENCH KEY (`benchKeyFor`), not by row index or object: rows are
// rebuilt from scratch on every save and every render, and a benchmark
// outlives both. A saved row's key is its id; an unsaved row's key is a
// draft id minted once and remembered against the row's own client-only
// `key` (stable across reorders and re-renders, unlike `data-idx`).
// The first four are per SLOT -- one key on one machine (`benchSlot`) --
// because one row can be benchmarked on several machines at once (the All
// view's Benchmark & tune does exactly that); the folds are per key.
let genBenchLast = {};    // the last FINISHED result seen for this slot (from the list, or live)
let genBenchLive = {};    // whatever the bench endpoint last said for this slot, any state
let genBenchNote = {};    // a message about this slot's button (a refusal, a lost connection)
let genBenchTimers = {};  // the poll in flight for a slot
let genBenchOpen = {};    // has the reader folded this key's result away
let genTrialsOpen = {};   // ... and its trials table
let genDraftBenchIds = {}; // row.key -> the draft bench id minted for it
// Whether the SERVER has bench endpoints at all: it answered 404 to the
// first ask. A button that can only 404 is worse than no button, and an
// error about something the user never pressed is noise, so once this is
// known the whole start/cancel affordance goes quiet everywhere.
let genBenchUnsupported = false;

// With a machine, the benchmark of this key ON THAT MACHINE ("local" is this
// server): one row can be measured on several, and the worker queues
// automatic benchmarks of its own, so a read or a Cancel that names no
// machine could find somebody else's.
function benchPath(key, machine) {
    const base = '/ocr/generations/' + encodeURIComponent(key) + '/bench';
    return machine ? base + '?processor=' + encodeURIComponent(machine) : base;
}

// One key's benchmark on one machine ("local" is this server). The All
// view's note about the whole run lives in the key's GEN_ALL slot.
function benchSlot(key, machine) {
    return key + '\n' + (machine || 'local');
}

function benchSlotKey(slot) {
    return slot.slice(0, slot.indexOf('\n'));
}

function benchSlotMachine(slot) {
    return slot.slice(slot.indexOf('\n') + 1);
}

function benchSlotsOf(key, map) {
    return Object.keys(map).filter((slot) => benchSlotKey(slot) === key);
}

// ---- keys -----------------------------------------------------------------

function genRandomDraftSuffix() {
    const chars = 'abcdefghijklmnopqrstuvwxyz0123456789';
    let out = '';
    for (let i = 0; i < 8; i++) out += chars[Math.floor(Math.random() * chars.length)];
    return out;
}

// The generation id once the row has one; otherwise a draft id minted once
// per row (keyed by the row's own stable `key`, not its position) and kept
// for as long as the row exists on this page.
function benchKeyFor(row) {
    if (row.id) return row.id;
    let id = genDraftBenchIds[row.key];
    if (!id) {
        id = 'draft-' + genRandomDraftSuffix();
        genDraftBenchIds[row.key] = id;
    }
    return id;
}

// ---- state ------------------------------------------------------------

// The list's `bench` is this server's last finished result (or whichever
// machine it names) with its trials stripped. A full one we watched finish
// is strictly richer, so it is kept when it is the same run. Only real ids
// ride the list; a draft key's state lives entirely in genBenchLive/
// genBenchLast until the row is saved or dropped.
function adoptBenchSummaries(generations) {
    const present = {};
    genRows.forEach((row) => { present[benchKeyFor(row)] = true; });
    generations.forEach((g) => {
        if (!g.id) return;
        present[g.id] = true;
        if (g.bench) {
            const slot = benchSlot(g.id, benchMeasuredOn(g.bench));
            const mine = genBenchLast[slot];
            const same = mine && mine.trials && mine.started_at && mine.started_at === g.bench.started_at;
            if (!same) genBenchLast[slot] = g.bench;
        } else {
            benchSlotsOf(g.id, genBenchLast).forEach((slot) => {
                const mine = genBenchLast[slot];
                if (!(mine && mine.state === 'done' && mine.trials)) delete genBenchLast[slot];
            });
        }
    });
    // A key that belongs to no current row -- a saved row that was removed,
    // or a draft row that was removed or saved (and so traded its draft key
    // for a real id) -- takes its benchmark tracking with it.
    [genBenchLast, genBenchLive, genBenchNote].forEach((map) => {
        Object.keys(map).forEach((slot) => { if (!present[benchSlotKey(slot)]) delete map[slot]; });
    });
    [genBenchOpen, genTrialsOpen].forEach((map) => {
        Object.keys(map).forEach((key) => { if (!present[key]) delete map[key]; });
    });
    Object.keys(genBenchTimers).forEach((slot) => { if (!present[benchSlotKey(slot)]) benchStop(slot); });
    Object.keys(genDraftBenchIds).forEach((rowKey) => {
        if (!genRows.some((row) => row.key === rowKey)) delete genDraftBenchIds[rowKey];
    });
}

// The machine a card's single-machine benchmark block is about.
function benchLive(row) {
    return genBenchLive[benchSlot(benchKeyFor(row), genPoolMachine(row))] || null;
}

// The machines the card's benchmark block speaks for: all of them in the All
// view, else the one it shows.
function benchViewMachines(row) {
    return genIsAll(row) ? genAllMachines() : [genMachine(row)];
}

function benchActiveMachines(row) {
    const key = benchKeyFor(row);
    return benchViewMachines(row).filter((machine) => benchIsActive(genBenchLive[benchSlot(key, machine)]));
}

// The machines "Benchmark & tune all" asks: this server while it does OCR of
// its own, and every connected processor with the row's engine installed.
// The server still has the last word on each (a missing detector, a pin to a
// card the machine has not got) and says why it refuses.
function benchEligibleMachines(row) {
    const out = genLocalProcessing ? ['local'] : [];
    // A processor has only the runner's --bench, which needs a staged
    // pipeline: a one-pass engine is measured on this server alone.
    if (!genIsMonolithic(row)) {
        genProcessors.forEach((p) => {
            const engines = (p.catalog && p.catalog.engines) || [];
            if (engines.indexOf(row.engine) !== -1) out.push(p.name);
        });
    }
    // A machine whose card cannot run the row's mode is never benchmarked
    // for it (a forced format it does not support).
    return out.filter((machine) => {
        const entry = genPrecisionModes(row).length ? genModeOn(row, machine) : null;
        return !entry || entry.eligible !== false;
    });
}

function benchIsActive(bench) {
    return !!bench && (bench.state === 'queued' || bench.state === 'running');
}

// The result to show for one machine: one we watched finish (it has the
// trials), else the last one known (the list's, for a real id). A failed or
// cancelled run does not erase it.
function benchResultFor(key, machine) {
    const slot = benchSlot(key, machine);
    const live = genBenchLive[slot];
    if (live && live.state === 'done') return live;
    return genBenchLast[slot] || null;
}

// ... for the machine the card shows. With no machine to choose, the one
// result there is, wherever it was measured (it says where). The All view has
// no single result: it adds them up (`benchAllResultHtml`).
function benchResult(row) {
    const key = benchKeyFor(row);
    if (!genHasMachineChoice(row)) {
        const here = benchResultFor(key, 'local');
        if (here) return here;
        const other = benchSlotsOf(key, genBenchLast).concat(benchSlotsOf(key, genBenchLive))
            .map((slot) => benchResultFor(key, benchSlotMachine(slot))).find(Boolean);
        return other || null;
    }
    const machine = genMachine(row);
    return machine === GEN_ALL ? null : benchResultFor(key, machine);
}

function benchOrdinal(n) {
    if (typeof n !== 'number' || !isFinite(n)) return String(n);
    const v = n % 100;
    if (v >= 10 && v <= 20) return n + 'th';
    const suffix = { 1: 'st', 2: 'nd', 3: 'rd' }[n % 10] || 'th';
    return n + suffix;
}

// ---- polling ------------------------------------------------------------

function benchStop(slot) {
    if (genBenchTimers[slot]) clearTimeout(genBenchTimers[slot]);
    delete genBenchTimers[slot];
}

// A row leaving the page (removed before it was ever saved) takes its client
// side tracking with it -- the benchmark itself, if any, keeps running on the
// server; nothing here is left polling for it.
function benchForget(key) {
    benchSlotsOf(key, genBenchTimers).forEach(benchStop);
    [genBenchLive, genBenchLast, genBenchNote].forEach((map) => {
        benchSlotsOf(key, map).forEach((slot) => { delete map[slot]; });
    });
    delete genBenchOpen[key];
    delete genTrialsOpen[key];
}

function benchSchedule(slot) {
    benchStop(slot);
    if (!benchIsActive(genBenchLive[slot])) return;
    if (document.hidden) return;
    genBenchTimers[slot] = setTimeout(() => benchPoll(slot), BENCH_POLL_MS);
}

async function benchPoll(slot) {
    delete genBenchTimers[slot];
    const key = benchSlotKey(slot);
    if (!genRows.some((row) => benchKeyFor(row) === key)) return;
    try {
        benchAdopt(slot, await apiGet(benchPath(key, benchSlotMachine(slot))));
    } catch (err) {
        // A key this server no longer knows (a draft's result is memory-only
        // and does not survive a restart) reads as idle, quietly -- not as a
        // benchmark that was lost.
        delete genBenchLive[slot];
        if (!err || err.status !== 404) {
            genBenchNote[slot] = 'Lost track of the benchmark: ' + err.message;
        }
        benchRefreshAll();
        return;
    }
    benchSchedule(slot);
}

function benchAdopt(slot, data) {
    if (!data || typeof data !== 'object') return;
    if (data.state === 'idle') delete genBenchLive[slot];
    else genBenchLive[slot] = data;
    // Filed under the machine that measured it, which is the slot's own.
    if (data.state === 'done') genBenchLast[benchSlot(benchSlotKey(slot), benchMeasuredOn(data))] = data;
    // A confirmed queued/running state IS the answer a stale note ("already
    // queued", "lost track of it") was standing in for -- the progress UI
    // takes over and the note would only repeat itself beside it.
    if (data.state === 'queued' || data.state === 'running') delete genBenchNote[slot];
    benchRefreshAll();
}

// Asks every row's key whether it has a benchmark going. This is what makes
// a reload mid-run resume, and it quietly notices a server with no bench
// endpoints at all (one 404, checked once).
function benchProbeAll() {
    if (genBenchUnsupported) return;
    genRows.forEach((row) => benchProbeRow(row));
}

// One row's benchmark on the machine the card shows -- on every machine, in
// the All view.
function benchProbeRow(row) {
    if (genBenchUnsupported) return;
    const key = benchKeyFor(row);
    benchViewMachines(row).forEach((machine) => {
        const slot = benchSlot(key, machine);
        apiGet(benchPath(key, machine)).then((data) => {
            benchAdopt(slot, data);
            benchSchedule(slot);
        }).catch((err) => {
            if (!err || err.status !== 404) return;
            // A draft key legitimately 404s until it is ever posted; that is
            // not "this server has no benchmarks". Only a REAL id 404ing
            // means the endpoint itself is missing.
            if (key.indexOf('draft-') === 0) return;
            if (genBenchUnsupported) return;
            genBenchUnsupported = true;
            genBenchNote = {};
            benchRefreshAll();
        });
    });
}

function onBenchVisibility() {
    if (document.hidden) {
        Object.keys(genBenchTimers).forEach(benchStop);
        return;
    }
    Object.keys(genBenchLive).forEach((slot) => {
        if (benchIsActive(genBenchLive[slot])) benchPoll(slot);
    });
}

// ---- actions ------------------------------------------------------------

async function startBench(index) {
    const row = genRows[index];
    if (!row) return;
    const key = benchKeyFor(row);
    const all = genIsAll(row);
    // On the machine the card shows (spec section 5): this server's own
    // hardware or one connected processor -- or, in the All view, every
    // machine that can run the row, each in its own machine's line.
    const machines = (all ? benchEligibleMachines(row) : [genPoolMachine(row)])
        .filter((machine) => !benchIsActive(genBenchLive[benchSlot(key, machine)]));
    delete genBenchNote[benchSlot(key, GEN_ALL)];
    if (!machines.length) {
        if (all) {
            genBenchNote[benchSlot(key, GEN_ALL)] = 'No connected machine can run this generation.';
            benchRefreshAll();
        }
        return;
    }
    // Optimistic: the button has to stop being a button the moment it is
    // pressed, or it gets pressed twice.
    machines.forEach((machine) => {
        const slot = benchSlot(key, machine);
        delete genBenchNote[slot];
        genBenchLive[slot] = {
            state: 'queued', generation: key, waiting_for_queue: false, position: null,
            processor: machine,
        };
    });
    benchRefreshAll();
    await Promise.all(machines.map((machine) => benchPost(row, key, machine)));
}

async function benchPost(row, key, machine) {
    const slot = benchSlot(key, machine);
    try {
        const request = { spec: genSpecFor(row, machine) };
        if (machine !== 'local') request.processor = machine;
        benchAdopt(slot, await apiPost(benchPath(key), request));
        benchSchedule(slot);
    } catch (err) {
        delete genBenchLive[slot];
        // ADDENDUM 6: 409 means only "this SAME key is already queued or
        // running" (on that machine) -- shown in the server's own words, same
        // as any other refusal. Posting the same key twice should not happen
        // through this button (it is already a Cancel by then), so this is a
        // race; also re-probe to pick up whatever the server says that key is
        // actually doing, which replaces the note with the real progress once
        // it lands (`benchAdopt` clears a stale note for an active state).
        genBenchNote[slot] = err.message;
        if (err.status === 409) benchProbeAll();
        benchRefreshAll();
    }
}

// Cancels what the card's block shows running: that machine's benchmark, or
// in the All view every machine's.
async function cancelBench(index) {
    const row = genRows[index];
    if (!row) return;
    const key = benchKeyFor(row);
    await Promise.all(benchActiveMachines(row).map(async (machine) => {
        const slot = benchSlot(key, machine);
        benchStop(slot);
        try {
            benchAdopt(slot, await apiDelete(benchPath(key, machine)));
        } catch (err) {
            genBenchNote[slot] = 'Could not cancel: ' + err.message;
            benchRefreshAll();
            benchSchedule(slot);
        }
    }));
}

// One click: the row takes the measured widths, right here in the editor.
// This does NOT save -- the settings only take effect once the list is
// saved, same as any other edit (ADDENDUM 5: "Apply never saves by itself
// any more").
function applyBench(index) {
    const row = genRows[index];
    if (!row) return;
    const result = benchResult(row);
    const best = result && result.best;
    if (!best) return;
    // A processor's widths belong to THAT machine and nowhere else: while it
    // is not connected there is no table of its to put them in, and the
    // row's own -- the config default, and this server's pools -- is never
    // the fallback (the button is not offered then either).
    if (!benchCanApply(result)) return;
    const measured = benchMeasuredOn(result);
    const applied = {
        stage_workers: Object.assign({}, best.stage_workers),
        queue_capacity: Object.assign({}, best.queue_capacity),
        stage_device: Object.assign({}, best.stage_device),
    };
    if (measured !== 'local') {
        // Measured on a processor: its pools (saved with "Save for ...").
        row.machine = measured;
        row.machinePools[measured] = applied;
    } else {
        row.pools = applied;
    }
    // Open the pools, so what was just applied is on screen and not behind a
    // disclosure the user has to go and find.
    row.open = true;
    renderGenerations();
    // A placement the tuner found moves stages between the CPU and the card,
    // which changes what their Workers cells may be: ask the server again.
    genDeriveStages(row);
}

// A fold's `toggle` event arrives a task after the click, so a re-render
// right behind it (a quick switch of the Machine select) would read the old
// state and shut it again: read the row's folds off the page first.
function genSyncFolds(index) {
    const row = genRows[index];
    const item = document.querySelector('#gen-list .gen[data-idx="' + index + '"]');
    if (!row || !item) return;
    const fold = (selector) => {
        const details = item.querySelector(selector);
        return details ? details.open : null;
    };
    const tuning = fold('details.gen__tuning');
    const why = fold('details.gen__why');
    const history = fold('details.gen__history');
    if (tuning !== null) row.open = tuning;
    if (why !== null) row.whyOpen = why;
    if (history !== null) row.historyOpen = history;
}

function onGenToggle(event) {
    const details = event.target;
    if (!details || !details.classList) return;
    // The pools panel is where a device is chosen, and choosing one re-renders
    // the row: without remembering that it was open, the table would shut
    // under the hands of whoever is tuning it.
    if (details.classList.contains('gen__tuning')) {
        const open = genRows[parseInt(details.dataset.idx, 10)];
        if (open) open.open = details.open;
        return;
    }
    if (details.classList.contains('gen__why')) {
        const owner = genRows[parseInt(details.dataset.idx, 10)];
        if (owner) owner.whyOpen = details.open;
        return;
    }
    if (details.classList.contains('gen__history')) {
        const owner = genRows[parseInt(details.dataset.idx, 10)];
        if (owner) owner.historyOpen = details.open;
        return;
    }
    const key = details.dataset.benchId;
    if (!key) return;
    if (details.classList.contains('bench-trials')) genTrialsOpen[key] = details.open;
    else if (details.classList.contains('bench-res')) genBenchOpen[key] = details.open;
}

// ---- rendering ----------------------------------------------------------

// Replaces every row's benchmark block in place. A poll must not re-render
// the list: that would take the caret out of whatever field is being typed
// in, and re-run the derived-name machinery for no reason.
function benchRefreshAll() {
    const list = document.getElementById('gen-list');
    if (!list) return;
    // Cancel is the one thing in here worth keeping hold of, and a poll every
    // couple of seconds would otherwise drop a keyboard user out of it.
    const active = document.activeElement;
    const held = active && list.contains(active) && active.closest('.gen__bench')
        ? '[data-act="' + active.dataset.act + '"][data-idx="' + active.dataset.idx + '"]'
        : null;
    genRows.forEach((row, index) => {
        const item = list.querySelector('.gen[data-idx="' + index + '"]');
        if (!item) return;
        // Only when it says something new: the Processors card refreshes
        // these every few seconds, and a block rebuilt for nothing would drop
        // whatever text someone had selected in it.
        const block = item.querySelector('.gen__bench');
        const html = genBenchHtml(row, index);
        if (block && block.dataset.html !== html) {
            block.outerHTML = html;
            const fresh = item.querySelector('.gen__bench');
            if (fresh) fresh.dataset.html = html;
        }
        // History is updated in place, never replaced: someone may be
        // reading it open, or have it focused.
        const history = item.querySelector('.gen__history');
        if (history) {
            const parts = genHistoryParts(row);
            const summary = history.querySelector('.gen__history-summary');
            if (summary.textContent !== parts.summary) {
                summary.textContent = parts.summary;
                summary.title = parts.summary;
            }
            const body = history.querySelector('.gen__history-body');
            if (body.innerHTML !== parts.body) body.innerHTML = parts.body;
        }
    });
    if (held) {
        const again = list.querySelector('.gen__bench ' + held);
        if (again && !again.disabled) again.focus();
    }
}

function genBenchHtml(row, index) {
    if (genBenchUnsupported) {
        // No endpoint here: whatever result the list carried still stands,
        // and nothing offers to start one.
        return '<div class="gen__bench">' + benchResultHtml(row, index) + '</div>';
    }
    const all = genIsAll(row);
    const live = all ? null : benchLive(row);
    const active = all ? benchActiveMachines(row).length > 0 : benchIsActive(live);
    return (
        '<div class="gen__bench">' +
        (active
            ? (all ? benchProgressAllHtml(row, index) : benchProgressHtml(live, index))
            : benchStartHtml(row, index)) +
        benchNoteHtml(row) +
        (active ? '' : benchOutcomeHtml(row)) +
        benchResultHtml(row, index) +
        '</div>'
    );
}

// Never disabled: a benchmark measures the row exactly as it is shown, saved
// or not, enabled or not, primary or not, while any OTHER row is queued or
// running (ADDENDUM 5 & 6). The only thing that takes this button away is
// THIS row's own benchmark already being active, which is handled by
// rendering `benchProgressHtml` in its place instead.
function benchStartHtml(row, index) {
    const tunable = !genIsMonolithic(row);
    const all = genIsAll(row);
    const label = (tunable ? 'Benchmark & tune' : 'Benchmark') + (all ? ' all' : '');
    let blurb;
    if (all) {
        const machines = benchEligibleMachines(row);
        blurb = 'Benchmarks ' + (tunable ? 'and tunes ' : '') + 'this generation on every machine that can run it (' +
            (machines.length ? machines.map(genMachineLabel).join(', ') : 'none is connected') + '), ' +
            'each on real pages from your library and in its own machine\'s line' +
            (tunable ? ', trying wider pools to find the fastest settings for each' : '') +
            '. Each machine measures the row exactly as shown here, unsaved edits included, with its own pools. ' +
            'Each machine pauses its OCR queue, and any volumes it was running restart when its benchmarks finish.';
    } else {
        const tuning = tunable ? ', then tries wider pools to find the fastest settings for this machine' : '';
        blurb = 'Times this generation on real pages from your library' + tuning +
            '. It measures the row exactly as shown here, unsaved edits included. The OCR queue ' +
            'pauses, and any volumes it was running restart when the benchmarks finish; you can ' +
            'queue more benchmarks behind this one.';
    }
    // What pressing it does is the button's tooltip and its accessible
    // description, not a paragraph repeated on every row.
    const whyId = 'bench-why-' + index;
    return (
        '<div class="bench-bar">' +
        '<button type="button" class="btn btn--secondary btn--sm bench-bar__btn" data-act="bench-start" data-idx="' + index + '"' +
        ' title="' + escapeHtml(blurb) + '" aria-describedby="' + whyId + '">' + label + '</button>' +
        '<span class="bench-bar__why sr-only" id="' + whyId + '">' + escapeHtml(blurb) + '</span>' +
        '</div>'
    );
}

function benchNoteHtml(row) {
    const key = benchKeyFor(row);
    const all = genIsAll(row);
    const notes = [];
    if (all) {
        const whole = genBenchNote[benchSlot(key, GEN_ALL)];
        if (whole) notes.push(whole);
        genAllMachines().forEach((machine) => {
            const note = genBenchNote[benchSlot(key, machine)];
            if (note) notes.push(genMachineLabel(machine) + ': ' + note);
        });
    } else {
        const note = genBenchNote[benchSlot(key, genPoolMachine(row))];
        if (note) notes.push(note);
    }
    return notes.map((note) =>
        '<p class="bench-bar__note" role="alert">' + escapeHtml(note) + '</p>').join('');
}

// A run that did not produce numbers still has to account for itself, and
// must not be mistaken for the result below it. In the All view, each
// machine's that ended that way, by name.
function benchOutcomeHtml(row) {
    if (!genIsAll(row)) return benchOutcomeLine(benchLive(row), '');
    const key = benchKeyFor(row);
    return genAllMachines().map((machine) =>
        benchOutcomeLine(genBenchLive[benchSlot(key, machine)], ' on ' + genMachineLabel(machine))).join('');
}

function benchOutcomeLine(live, where) {
    if (!live) return '';
    if (live.state === 'failed') {
        return '<p class="bench-out bench-out--failed" role="alert">Benchmark failed' + escapeHtml(where) + ': ' +
            escapeHtml(live.error || 'no reason given') + '</p>';
    }
    if (live.state === 'cancelled') {
        return '<p class="bench-out">Benchmark cancelled' + escapeHtml(where) + '.</p>';
    }
    return '';
}

// What THIS row's own bench is doing: pausing the OCR queue, waiting its
// turn behind others, walking trials, or done. `live.position` is the FIFO
// place ADDENDUM 6 defines: 0 = running, 1 = next, 2 = the one after, ...
function benchProgressHtml(live, index) {
    const progress = live.progress || null;
    let head;
    // The worker's own benchmark of a machine it has not measured yet: said,
    // so a Cancel here is a decision and not an accident.
    const auto = live.autobench
        ? 'Automatic benchmark on ' + genMachineLabel(benchMeasuredOn(live)) + ' — '
        : '';
    if (live.state === 'queued') {
        head = 'Queued — ' + benchOrdinal(live.position || 1) + ' in line';
    } else if (live.waiting_for_queue) {
        head = 'Pausing the OCR queue…';
    } else if (progress && typeof progress.trial === 'number') {
        head = 'Trial ' + progress.trial + ' of ' + (progress.max_trials || '?') +
            ' — ' + benchWidths(progress.stage_workers);
    } else {
        head = 'Running…';
    }
    head = auto + head;

    const parts = [];
    if (progress && typeof progress.pages === 'number' && progress.pages > 0) {
        parts.push(benchThousands(progress.pages_done || 0) + ' of ' + benchThousands(progress.pages) + ' pages'
            // A trial runs the sample again until the window is long enough
            // to mean something, so "pages" alone would look stuck.
            + (typeof progress.pass_index === 'number' && progress.pass_index > 1
                ? ', pass ' + progress.pass_index : ''));
    }
    const rate = benchRatePerSecond(progress && progress.pages_per_second);
    if (rate) parts.push(rate);
    const windowSoFar = progress && typeof progress.window_seconds === 'number'
        ? benchDuration(progress.window_seconds) : null;
    if (windowSoFar) parts.push(windowSoFar + ' window');

    const pct = progress && progress.pages
        ? Math.max(0, Math.min(100, Math.round((progress.pages_done || 0) / progress.pages * 100)))
        : null;

    return (
        '<div class="bench-run">' +
        '<div class="bench-run__head">' +
        '<span class="bench-run__spin" aria-hidden="true"></span>' +
        '<span class="bench-run__title" role="status" aria-live="polite">' + escapeHtml(head) + '</span>' +
        '<button type="button" class="btn btn--secondary btn--sm bench-run__cancel" data-act="bench-cancel" data-idx="' + index + '">Cancel</button>' +
        '</div>' +
        (pct === null
            ? ''
            : '<div class="bench-run__track" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="' + pct + '">' +
              '<span class="bench-run__fill" style="width:' + pct + '%"></span></div>') +
        (parts.length ? '<p class="bench-run__nums">' + escapeHtml(parts.join(' · ')) + '</p>' : '') +
        '</div>'
    );
}

// The All view's run: one line saying how many machines are being
// benchmarked, one Cancel for all of them, and each machine's state.
function benchProgressAllHtml(row, index) {
    const key = benchKeyFor(row);
    const shown = genAllMachines()
        .map((machine) => ({ machine: machine, live: genBenchLive[benchSlot(key, machine)] }))
        .filter((entry) => entry.live && (benchIsActive(entry.live) || entry.live.state === 'done'));
    const parts = shown.map((entry) => genMachineLabel(entry.machine) + ': ' + benchShortState(entry.live));
    const head = 'Benchmarking ' + genCount(shown.length, 'machine');
    return (
        '<div class="bench-run bench-run--all">' +
        '<div class="bench-run__head">' +
        '<span class="bench-run__spin" aria-hidden="true"></span>' +
        '<span class="bench-run__title" role="status" aria-live="polite" title="' + escapeHtml(head + ' — ' + parts.join(' · ')) + '">' +
        escapeHtml(head) + '</span>' +
        '<button type="button" class="btn btn--secondary btn--sm bench-run__cancel" data-act="bench-cancel" data-idx="' + index + '"' +
        ' title="Cancel every machine\'s benchmark of this generation">Cancel all</button>' +
        '</div>' +
        '<p class="bench-run__nums">' + escapeHtml(parts.join(' · ')) + '</p>' +
        '</div>'
    );
}

// One machine's part of an All run, in a few words.
function benchShortState(live) {
    if (live.state === 'done') return 'done';
    if (live.state === 'queued') return 'queued, ' + benchOrdinal(live.position || 1) + ' in line';
    if (live.waiting_for_queue) return 'pausing its OCR queue';
    const progress = live.progress || null;
    if (progress && typeof progress.trial === 'number') {
        return 'trial ' + progress.trial + ' of ' + (progress.max_trials || '?');
    }
    return 'running';
}

function benchHeadlineRate(result) {
    if (result.best && typeof result.best.pages_per_second === 'number') return result.best.pages_per_second;
    if (result.baseline && typeof result.baseline.pages_per_second === 'number') return result.baseline.pages_per_second;
    return null;
}

function benchResultHtml(row, index) {
    if (genIsAll(row)) return benchAllResultHtml(row);
    const result = benchResult(row);
    const rate = result ? benchHeadlineRate(result) : null;
    const headline = benchRatePerMinute(rate);
    const best = (result && result.best) || {};
    // A run whose window was too short produced no usable rate, and that is
    // a result worth showing: it says what to do about it (more pages), and
    // hiding it would leave the row looking as though nothing had run.
    if (!result || (!headline && !benchShortWindow(best))) return benchNoResultHtml(row);

    const applied = result.tunable === false || benchAlreadyApplied(row, best, result.processor);
    const tuned = result.tunable !== false && benchHasPools(best) &&
        typeof best.speedup === 'number' && best.speedup > 1.005;

    const perPage = benchSecondsPerPage(result);
    const sub = [];
    if (perPage) sub.push(perPage);
    sub.push(tuned ? 'with the tuned settings below' : 'as measured');
    const busy = benchBusyText(best);
    if (busy) sub.push(busy);

    // The summary is the answer -- what won, how fast, how long ago -- on
    // one line, cut to fit and whole in its tooltip; the body says the rest.
    const line = benchSummaryText(result);

    return (
        '<details class="bench-res" data-bench-id="' + escapeHtml(benchKeyFor(row)) + '"' +
        (benchResultOpen(row, result, applied) ? ' open' : '') + '>' +
        '<summary class="bench-res__summary" title="' + escapeHtml(line) + '">' + escapeHtml(line) + '</summary>' +
        '<div class="bench-res__body">' +
        '<p class="bench-res__lead"><strong class="bench-res__rate">' + escapeHtml(headline || 'No usable rate') + '</strong>' +
        '<span class="bench-res__sub">' + escapeHtml(sub.join(' · ')) + '</span></p>' +
        benchWindowHtml(best) +
        benchSpecNoticeHtml(result, row) +
        benchPreemptedHtml(result) +
        benchFactsHtml(result, row) +
        benchHostHtml(result) +
        benchStoryHtml(result, row, index, applied) +
        '</div></details>'
    );
}

// "Benchmark result — best: detect ×3 · 126 pages/min · 2 d ago".
function benchSummaryText(result) {
    const best = result.best || {};
    const bits = [];
    if (benchShortWindow(best)) {
        bits.push('window too short to tune on');
    } else if (result.tunable !== false) {
        bits.push('best: ' + benchBestLabel(best));
    }
    const rate = benchRatePerMinute(benchHeadlineRate(result));
    if (rate) bits.push(rate);
    const when = result.finished_at || result.started_at;
    const ago = when && !isNaN(Date.parse(when)) ? agoShort(Date.parse(when) / 1000) : '';
    if (ago) bits.push(ago);
    return 'Benchmark result' + (bits.length ? ' — ' + bits.join(' · ') : '');
}

// Where a card that can switch machines has no result for the one it shows,
// the slot still holds a line, so switching moves nothing below it.
function benchNoResultHtml(row) {
    if (!genHasMachineChoice(row)) return '';
    return '<p class="bench-res bench-res--none"><span class="bench-res__none">Benchmark result — none on ' +
        escapeHtml(genMachineLabel(genMachine(row))) + ' yet</span></p>';
}

// One machine's benchmark of a row in pages a minute, and when: the full
// result when this page has it, else the figure the server keeps for that
// machine (its profile, or the Processors card's Benchmark number).
function benchMachineRate(row, machine) {
    const result = benchResultFor(benchKeyFor(row), machine);
    const pps = result ? benchHeadlineRate(result) : null;
    if (pps) return { ppm: pps * 60, at: result.finished_at || result.started_at || null };
    const flat = machine === 'local' ? row.localBench : row.processorBench[machine];
    if (flat && typeof flat.pages_per_second === 'number' && flat.pages_per_second > 0) {
        return { ppm: flat.pages_per_second * 60, at: flat.at || null };
    }
    const layer = genSpeedLayer(row, machine);
    if (layer && typeof layer.bench_pages_per_minute === 'number' && layer.bench_pages_per_minute > 0) {
        return { ppm: layer.bench_pages_per_minute, at: null };
    }
    return null;
}

// The All view's result: every machine's benchmark added up (combined
// throughput), and which machine contributed what. A machine's full result,
// its trials and its Apply are one pick of the Machine select away.
function benchAllResultHtml(row) {
    const key = benchKeyFor(row);
    const machines = genAllMachines().map((machine) => ({ machine: machine, rate: benchMachineRate(row, machine) }));
    const measured = machines.filter((entry) => entry.rate);
    if (!measured.length) {
        return '<p class="bench-res bench-res--none"><span class="bench-res__none">Benchmark results — none yet</span></p>';
    }
    const total = measured.reduce((sum, entry) => sum + entry.rate.ppm, 0);
    const line = 'Benchmark results — ' + genPpmText(total) + ' pages/min' +
        (measured.length > 1 ? ' combined' : '') + ' · ' + measured.length + ' of ' +
        genCount(machines.length, 'machine');
    const items = machines.map((entry) => {
        const label = '<span class="bench-res__machine">' + escapeHtml(genMachineLabel(entry.machine)) + '</span> ';
        if (!entry.rate) return '<li data-machine="' + escapeHtml(entry.machine) + '">' + label + 'not benchmarked</li>';
        const when = entry.rate.at && !isNaN(Date.parse(entry.rate.at))
            ? ' · ' + agoShort(Date.parse(entry.rate.at) / 1000) : '';
        return '<li data-machine="' + escapeHtml(entry.machine) + '">' + label + '<strong>' +
            escapeHtml(genPpmText(entry.rate.ppm)) + ' pages/min</strong>' + escapeHtml(when) + '</li>';
    }).join('');
    return (
        '<details class="bench-res bench-res--all" data-bench-id="' + escapeHtml(key) + '"' +
        (genBenchOpen[key] ? ' open' : '') + '>' +
        '<summary class="bench-res__summary" title="' + escapeHtml(line) + '">' + escapeHtml(line) + '</summary>' +
        '<div class="bench-res__body"><ul class="bench-res__machines">' + items + '</ul>' +
        '<p class="bench-res__pick">Pick a machine to see its full result.</p></div></details>'
    );
}

// Unfolded while it is worth acting on: a run that just finished, or one
// offering settings this row does not have yet. A result with nothing left
// to do folds away and leaves its headline on the row's one-glance line.
function benchResultOpen(row, result, applied) {
    const key = benchKeyFor(row);
    if (Object.prototype.hasOwnProperty.call(genBenchOpen, key)) return genBenchOpen[key];
    const live = benchLive(row);
    if (live && live.state === 'done') return true;
    return benchHasPools(result.best || {}) && !applied;
}

// What this benchmark measured (ADDENDUM 5) beside what the row shows right
// now. They usually agree; when they do not, that is said plainly and the
// numbers are shown anyway -- they are still real numbers, just not for the
// settings on screen at this moment.
function benchSpecNoticeHtml(result, row) {
    if (!result.spec) return '';
    const measuredOn = result.processor && result.processor !== 'local' ? result.processor : 'local';
    if (genSpecsEqual(genSpecWithMode(result.spec), genSpecWithMode(genSpecFor(row, measuredOn)))) return '';
    return '<p class="bench-res__stale" role="note">Measured with different settings than shown here: ' +
        escapeHtml(benchSpecSummary(result.spec)) + '.</p>';
}

function benchSpecSummary(spec) {
    const bits = [spec.engine || '?'];
    if (spec.detector) bits.push(spec.detector);
    if (typeof spec.patch_budget === 'number') bits.push('patch budget ' + spec.patch_budget);
    if (typeof spec.precision === 'string' && spec.precision) bits.push(genModeLabel(spec.precision));
    return bits.join(' · ');
}

// A spec that names no mode for an engine that takes one meant the default
// (a result measured before the mode existed), so the two compare as one.
function genSpecWithMode(spec) {
    const out = Object.assign({}, spec || {});
    const engine = genEngine(out.engine);
    const takes = !!engine && Array.isArray(engine.precision_modes) && engine.precision_modes.length > 0;
    if (takes && !out.precision) out.precision = genDefaultMode();
    if (!takes) delete out.precision;
    return out;
}

// What running this benchmark interrupted (ADDENDUM 5 & 6): stated once,
// plainly, never as an error -- these volumes simply run again from the
// start once every queued benchmark is done.
function benchPreemptedHtml(result) {
    const list = Array.isArray(result.preempted) ? result.preempted : [];
    if (!list.length) return '';
    const names = list.map((item) => escapeHtml(item.volume || item.generation || '')).filter(Boolean);
    const text = 'Interrupted ' + genCount(list.length, 'running volume') +
        (names.length ? ' (' + names.join(', ') + ')' : '') +
        '; they restart when the benchmarks finish.';
    return '<p class="bench-res__preempted" role="note">' + escapeHtml(text) + '</p>';
}

// Every fact here is omitted when the platform could not measure it. A
// missing number is a thing this machine cannot say, never a "null".
function benchFactsHtml(result, row) {
    const estimates = result.estimates || {};
    const facts = [];
    // Startup is paid once per SESSION -- one open pipeline reads every queued
    // volume of this generation -- except for a monolithic engine, which is
    // still one invocation per volume and pays it every time.
    const perVolume = result.tunable === false || (row && genIsMonolithic(row));

    const volume = benchDuration(estimates.volume_200_pages_seconds);
    if (volume) {
        facts.push(['A ' + BENCH_VOLUME_PAGES + '-page volume', volume, benchVolumeNote(result, perVolume)]);
    }

    const remaining = benchDuration(estimates.remaining_seconds);
    if (remaining) {
        const pages = typeof estimates.remaining_pages === 'number'
            ? benchThousands(estimates.remaining_pages) + ' pages left' : '';
        // A duration is hard to act on; a clock time is not. The instant is
        // computed HERE, from the browser's own now, for the same reason the
        // queue page formats the server's UTC locally: this is the only side
        // that knows which zone anyone is reading in.
        const done = localFinishClock(estimates.remaining_seconds);
        facts.push([
            'The rest of the library for this generation',
            remaining,
            done ? (pages ? pages + ' — done by ' + done : 'done by ' + done) : pages,
        ]);
    }

    // ADDENDUM 9: startup is INFORMATION, never a term. It is what the
    // machine spent before the first page came out -- interpreter, imports,
    // model load, pipeline fill -- and it is in nothing above.
    const startup = benchDuration(result.startup_seconds);
    if (startup) {
        facts.push([
            'First page after',
            startup,
            perVolume
                ? 'loading, paid again for every volume — not counted in the speed above'
                : 'loading, paid once per session — not counted in the speed above',
        ]);
    }

    const vram = benchMegabytes(result.peak_vram_mb);
    if (vram) facts.push(['Peak VRAM', vram, '']);

    const rss = benchMegabytes(result.peak_rss_mb);
    if (rss) facts.push(['Peak RAM', rss, '']);

    if (!facts.length) return '';
    return '<ul class="bench-res__facts">' + facts.map((fact) =>
        '<li><span class="bench-res__fact-label">' + escapeHtml(fact[0]) + '</span>' +
        '<strong class="bench-res__fact-value">' + escapeHtml(fact[1]) + '</strong>' +
        (fact[2] ? '<span class="bench-res__fact-note">' + escapeHtml(fact[2]) + '</span>' : '') +
        '</li>').join('') + '</ul>';
}

// ADDENDUM 9: an estimate is 200 / the measured rate and NOTHING else, so
// this no longer has to guess which of the two a number is. It says which
// one it is, and where the load a reader is about to ask about went.
function benchVolumeNote(result, perVolume) {
    return perVolume
        ? 'reading only — this engine loads again for every volume, on top'
        : 'reading only — the model load is paid once per session, not per volume';
}

// The window a rate was read over, said plainly -- and said as a WARNING
// when it was too short for the number to be worth comparing.
function benchWindowHtml(best) {
    const seconds = typeof best.window_seconds === 'number' ? best.window_seconds : null;
    if (seconds === null) return '';
    const pages = typeof best.pages_measured === 'number' ? best.pages_measured : null;
    const passes = typeof best.passes === 'number' && best.passes > 1 ? best.passes : null;
    const over = benchDuration(seconds) +
        (pages === null ? '' : ' of page results (' + benchThousands(pages) + ' pages' +
            (passes ? ' over ' + passes + ' passes' : '') + ')');
    if (!benchShortWindow(best)) {
        return '<p class="bench-res__window">' + escapeHtml('Timed over ' + over + '.') + '</p>';
    }
    return '<p class="bench-res__window bench-res__window--short" role="note">' +
        escapeHtml('Only ' + over + ' — too short to compare settings on, so nothing was ' +
            'tuned from it. Benchmark more pages.') + '</p>';
}

function benchShortWindow(best) {
    if (!best) return false;
    if (best.short_window === true) return true;
    if (typeof best.window_seconds !== 'number') return false;
    return best.window_seconds < BENCH_SHORT_WINDOW_SECONDS;
}

// "neither the GPU nor the CPU seemed tapped", as a number: how busy each
// was over the very window the rate was read from. Either may be absent --
// a machine that cannot say does not get a zero put in its mouth.
function benchBusyText(source) {
    const bits = [];
    if (typeof (source || {}).gpu_busy_pct === 'number') {
        bits.push('GPU ' + Math.round(source.gpu_busy_pct) + '% busy');
    }
    if (typeof (source || {}).cpu_busy_pct === 'number') {
        bits.push('CPU ' + Math.round(source.cpu_busy_pct) + '% busy');
    }
    return bits.join(' · ');
}

function benchHostHtml(result) {
    const host = result.host || {};
    const bits = [];
    if (host.cpu) bits.push(host.cpu);
    if (host.gpu) bits.push(host.gpu);
    if (host.backend) bits.push(host.backend + ' backend');
    const when = result.finished_at || result.started_at;
    const sample = result.sample || {};

    let text = 'Measured';
    if (when && !isNaN(Date.parse(when))) text += ' ' + genRelativeTime(when);
    if (result.processor && result.processor !== 'local') {
        text += ' on ' + genMachineLabel(result.processor) + (bits.length ? ' (' + bits.join(' · ') + ')' : '');
    } else if (bits.length) {
        text += ' on ' + bits.join(' · ');
    }
    if (typeof sample.pages === 'number') {
        text += ', over ' + genCount(sample.pages, 'page') +
            (typeof sample.volumes === 'number' ? ' from ' + genCount(sample.volumes, 'volume') : '') +
            ' of your library';
    }
    if (text === 'Measured') return '';
    return '<p class="bench-res__host">' + escapeHtml(text + '.') + '</p>';
}

// The tuning story: what was tried, what each trial showed, what was kept.
function benchStoryHtml(result, row, index, applied) {
    if (result.tunable === false) {
        return '<p class="bench-res__conclusion">This engine reads a volume in one pass, so it has no pools to tune — this is simply how fast it is here.</p>';
    }
    const best = result.best || {};
    const trials = Array.isArray(result.trials) ? result.trials : [];
    const worthApplying = benchHasPools(best) && !applied;
    const canApply = worthApplying && benchCanApply(result);
    const gone = worthApplying && !canApply
        ? '<p class="bench-res__gone" role="note">Measured on ' + escapeHtml(benchMeasuredOn(result)) +
          ', which is not connected: these settings are that machine’s, and can be applied to its pools once it is back.</p>'
        : '';

    return (
        (trials.length ? benchTrialsHtml(result, trials, row) : '') +
        '<p class="bench-res__conclusion">' + escapeHtml(benchConclusion(result, applied)) + '</p>' +
        gone +
        (canApply
            ? '<div class="bench-res__apply"><button type="button" class="btn btn--primary btn--sm" data-act="bench-apply" data-idx="' + index + '">Apply these settings</button>' +
              '<span class="bench-res__apply-why">Sets this generation’s pools to ' + escapeHtml(benchBestLabel(best)) +
              ' in the editor below — applied to this row, save the list to use it.</span></div>'
            : '')
    );
}

function benchTrialsHtml(result, trials, row) {
    const key = benchKeyFor(row);
    const open = genTrialsOpen[key];
    const rows = trials.map((trial) => {
        const rate = benchRatePerSecond(trial.pages_per_second) || '—';
        const verdict = trial.verdict || (trial.bottleneck ? trial.bottleneck + ' was busiest' : 'no stage singled out');
        const short = benchShortWindow(trial);
        // A step is only ever "reverted" on its own merits; one the window
        // was too short to read was not judged at all, and saying it lost
        // would be saying something the benchmark did not find out.
        const kept = short ? 'not decidable' : (trial.accepted === false ? 'reverted' : 'kept');
        const widths = benchWidths(trial.stage_workers, trial.stage_device);
        // A note that only repeats the widths beside it ("detect ×3") is
        // noise; one that says something else ("auto") is the label.
        const note = trial.note && widths.indexOf(trial.note) !== 0 ? trial.note : '';
        const busy = benchBusyText(trial);
        return (
            '<tr class="' + (trial.accepted === false ? 'bench-trials__row--out' : '') + '">' +
            '<td class="num" data-label="Trial">' + escapeHtml(String(trial.n == null ? '' : trial.n)) + '</td>' +
            '<th scope="row"><span class="mono">' + escapeHtml(widths) + '</span>' +
            (note ? '<span class="bench-trials__note">' + escapeHtml(note) + '</span>' : '') + '</th>' +
            '<td class="num" data-label="Speed">' + escapeHtml(rate) +
            (busy ? '<span class="bench-trials__busy">' + escapeHtml(busy) + '</span>' : '') + '</td>' +
            '<td class="num" data-label="Window">' + escapeHtml(benchTrialWindow(trial)) +
            (short ? '<span class="bench-trials__short">too short to decide on</span>' : '') + '</td>' +
            '<td>' + escapeHtml(verdict) + '</td>' +
            '<td>' + kept + '</td></tr>'
        );
    }).join('');
    return (
        '<details class="bench-trials" data-bench-id="' + escapeHtml(key) + '"' + (open ? ' open' : '') + '>' +
        '<summary>How it was tuned — ' + genCount(trials.length, 'trial') + '</summary>' +
        '<div class="table-container"><table class="bench-trials__table">' +
        '<thead><tr><th scope="col" class="num">#</th><th scope="col">Pool widths</th>' +
        '<th scope="col" class="num">Speed</th><th scope="col" class="num">Window</th>' +
        '<th scope="col">What it showed</th><th scope="col">Kept?</th></tr></thead>' +
        '<tbody>' + rows + '</tbody></table></div></details>'
    );
}

// A trial's window as the table shows it: how long, over how many passes.
function benchTrialWindow(trial) {
    if (typeof trial.window_seconds !== 'number') return '—';
    const passes = typeof trial.passes === 'number' && trial.passes > 1
        ? ' × ' + trial.passes + ' passes' : '';
    return (benchDuration(trial.window_seconds) || '—') + passes;
}

function benchConclusion(result, applied) {
    const best = result.best || {};
    if (benchShortWindow(best)) {
        return 'The pages came out in too short a window to compare settings — nothing was ' +
            'tuned. Run the benchmark over more pages.';
    }
    if (!benchHasPools(best)) {
        return 'Auto is already the best this machine can do — nothing to change.';
    }
    const speedup = typeof best.speedup === 'number' && best.speedup > 1.005
        ? ' is ' + benchSpeedup(best.speedup) + ' faster than auto on this machine'
        : ' was the fastest setting found';
    return benchBestLabel(best) + speedup + (applied ? ' — already applied.' : '.');
}

// `best` carries what differs from the derived widths (and the row's pins, as
// they ran), so this reads as
// the change to make, not as a full set of pool sizes.
function benchBestLabel(best) {
    const bits = [];
    const devices = best.stage_device || {};
    Object.keys(devices).forEach((key) => { bits.push(key + ' on ' + genDeviceShort(devices[key])); });
    const workers = best.stage_workers || {};
    // `auto`: a width the row pins that this machine runs derived.
    Object.keys(workers).forEach((key) => {
        bits.push(workers[key] === 'auto' ? key + ' auto' : key + ' ×' + workers[key]);
    });
    const capacity = best.queue_capacity || {};
    Object.keys(capacity).forEach((key) => { bits.push(key + ' queue ' + capacity[key]); });
    return bits.length ? bits.join(', ') : 'auto';
}

// Which machine a result was measured on: "local" (this server) or a
// processor's name.
function benchMeasuredOn(result) {
    return result && result.processor && result.processor !== 'local' ? result.processor : 'local';
}

// A result may be applied to the machine that measured it, and only while
// that machine has a table on this page: this server always, a processor
// while it is connected.
function benchCanApply(result) {
    const measured = benchMeasuredOn(result);
    return measured === 'local' || genProcessors.some((p) => p.name === measured);
}

function benchHasPools(best) {
    return Object.keys((best && best.stage_workers) || {}).length > 0 ||
        Object.keys((best && best.queue_capacity) || {}).length > 0 ||
        Object.keys((best && best.stage_device) || {}).length > 0;
}

// The row's own pools are fresher than the flag the server computed when the
// benchmark ran, so they decide; the flag settles it once the two agree.
function benchAlreadyApplied(row, best, machine) {
    const pools = genPoolsOf(row, machine || 'local');
    const same = benchPoolsEqual(pools.stage_workers, best.stage_workers) &&
        benchPoolsEqual(pools.queue_capacity, best.queue_capacity) &&
        benchDevicesEqual(pools.stage_device, best.stage_device);
    if (same) return true;
    return best.same_as_spec === true;
}

function benchDevicesEqual(a, b) {
    const left = genCleanDevices(a || {});
    const right = genCleanDevices(b || {});
    const keys = Object.keys(left);
    if (keys.length !== Object.keys(right).length) return false;
    return keys.every((key) => left[key] === right[key]);
}

function benchPoolsEqual(a, b) {
    const left = genCleanPool(a || {});
    const right = genCleanPool(b || {});
    const keys = Object.keys(left);
    if (keys.length !== Object.keys(right).length) return false;
    return keys.every((key) => left[key] === right[key]);
}

// ---- formatting ---------------------------------------------------------
// Nothing here ever reaches the page as a raw float of seconds or a count of
// megabytes: a number a person cannot picture is not an answer.

// "in 4 h 20 m" as the clock face it lands on, in THIS browser's zone and
// locale. The server never formats a local time -- it has no idea which zone
// anyone is reading in -- so, exactly as on the queue page, the conversion
// happens here. The date is added when the instant is not today, because a
// bare "02:15" on a run that ends tomorrow morning is the one way this could
// actively mislead.
function localFinishClock(seconds) {
    if (typeof seconds !== 'number' || !isFinite(seconds) || seconds < 0) return null;
    const when = new Date(Date.now() + seconds * 1000);
    const time = when.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    const now = new Date();
    if (
        when.getFullYear() === now.getFullYear() &&
        when.getMonth() === now.getMonth() &&
        when.getDate() === now.getDate()
    ) {
        return time;
    }
    return when.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ' ' + time;
}

function benchDuration(seconds) {
    if (typeof seconds !== 'number' || !isFinite(seconds) || seconds < 0) return null;
    if (seconds < 10) {
        const tenths = Math.round(seconds * 10) / 10;
        return (tenths === Math.round(tenths) ? String(tenths) : tenths.toFixed(1)) + ' s';
    }
    const total = Math.round(seconds);
    if (total < 60) return total + ' s';
    if (total < 3600) {
        const m = Math.floor(total / 60);
        const s = total % 60;
        return s ? m + ' m ' + s + ' s' : m + ' m';
    }
    const h = Math.floor(total / 3600);
    const m = Math.round((total % 3600) / 60);
    if (m === 60) return (h + 1) + ' h';
    return m ? h + ' h ' + m + ' m' : h + ' h';
}

function benchMegabytes(mb) {
    if (typeof mb !== 'number' || !isFinite(mb) || mb <= 0) return null;
    if (mb < 1024) return Math.round(mb) + ' MB';
    return (mb / 1024).toFixed(1) + ' GB';
}

function benchThousands(n) {
    if (typeof n !== 'number' || !isFinite(n)) return '0';
    return String(Math.round(n)).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

// Pages a minute is the unit a library is planned in; pages a second is the
// unit a trial is compared in. Both, in their own place.
function benchRatePerMinute(pagesPerSecond) {
    if (typeof pagesPerSecond !== 'number' || !isFinite(pagesPerSecond) || pagesPerSecond <= 0) return null;
    const perMinute = pagesPerSecond * 60;
    const shown = perMinute >= 10 ? Math.round(perMinute) : Math.round(perMinute * 10) / 10;
    return shown + ' pages/min';
}

function benchRatePerSecond(pagesPerSecond) {
    if (typeof pagesPerSecond !== 'number' || !isFinite(pagesPerSecond) || pagesPerSecond <= 0) return null;
    return (Math.round(pagesPerSecond * 100) / 100).toFixed(2) + ' pages/s';
}

function benchSecondsPerPage(result) {
    const best = result.best || {};
    let spp = typeof best.seconds_per_page === 'number' ? best.seconds_per_page : null;
    if (spp === null && typeof best.pages_per_second === 'number' && best.pages_per_second > 0) {
        spp = 1 / best.pages_per_second;
    }
    if (spp === null || !isFinite(spp) || spp <= 0) return null;
    if (spp < 10) return (Math.round(spp * 100) / 100).toFixed(2) + ' s per page';
    return benchDuration(spp) + ' per page';
}

function benchSpeedup(x) {
    return (Math.round(x * 100) / 100).toFixed(2) + '×';
}

// A trial is a placement AND a set of widths, so the line says both:
// "detect · CPU ×3, engine · GPU 0".
function benchWidths(map, devices) {
    const widths = map || {};
    const where = devices || {};
    const keys = Object.keys(widths);
    Object.keys(where).forEach((key) => {
        if (keys.indexOf(key) === -1) keys.push(key);
    });
    if (!keys.length) return 'auto';
    return keys.map((key) => {
        const device = where[key] ? ' · ' + genDeviceShort(where[key]) : '';
        const width = widths[key] == null ? '' : ' ×' + widths[key];
        return key + device + width;
    }).join(', ');
}

// ============================================
// Status Tab
// ============================================

async function loadStatus() {
    try {
        const data = await apiGet('/status');
        document.getElementById('status-uptime').textContent = formatUptime(data.uptime);
        document.getElementById('status-host').textContent = `${data.host}:${data.port}`;
        document.getElementById('status-users').textContent = data.user_count;
        document.getElementById('status-volumes').textContent = data.volume_count;
        document.getElementById('status-storage-path').textContent = data.storage_path;

        // Disk usage
        if (data.disk_total > 0) {
            const pct = ((data.disk_used / data.disk_total) * 100).toFixed(1);
            document.getElementById('status-disk-fill').style.width = pct + '%';
            document.getElementById('status-disk-used').textContent = formatBytes(data.disk_used) + ' used';
            document.getElementById('status-disk-total').textContent = formatBytes(data.disk_total) + ' total';
        }
    } catch (err) {
        showToast('Failed to load status: ' + err.message, 'error');
    }
}

// Updates: the server checks for releases itself (every 12 h when update.check is
// on); "Check now" asks again. A server without the endpoint (404) hides the card.
async function loadUpdate(refresh) {
    const card = document.getElementById('update-card');
    let data;
    try {
        data = await apiGet('/update' + (refresh ? '?refresh=1' : ''));
    } catch (err) {
        if (err.status === 404) {
            card.hidden = true;
        } else {
            showToast('Update check failed: ' + err.message, 'error');
        }
        return;
    }
    card.hidden = false;
    let summary = 'Running ' + data.current + '. ';
    if (data.error) {
        summary += 'Could not check for updates: ' + data.error;
    } else if (data.available) {
        summary += 'Version ' + data.latest + ' is available.';
    } else if (data.latest) {
        summary += 'This is the latest release.';
    } else {
        summary += data.checks_enabled === false ? 'Automatic update checks are off.' : 'Not checked yet.';
    }
    document.getElementById('update-summary').textContent = summary;
    const hint = document.getElementById('update-hint');
    hint.textContent = data.available && !data.can_apply ? (data.cannot_apply_reason || '') : '';
    hint.hidden = !hint.textContent;
    const apply = document.getElementById('update-apply-btn');
    // .btn sets display, which beats the hidden attribute.
    apply.style.display = data.available && data.can_apply ? '' : 'none';
    apply.disabled = !!data.applying;
    const notes = document.getElementById('update-notes');
    notes.style.display = data.notes_url ? '' : 'none';
    if (data.notes_url) notes.href = data.notes_url;
}

async function applyUpdate() {
    if (!confirm('Download and install the update, then restart the server?')) return;
    const apply = document.getElementById('update-apply-btn');
    apply.disabled = true;
    try {
        const result = await apiPost('/update/apply', {});
        showToast('Installed ' + result.version + (result.restarting
            ? '; the server is restarting.' : '. Restart the server to finish.'), 'success');
        if (result.restarting) setTimeout(() => window.location.reload(), 8000);
    } catch (err) {
        showToast('Update failed: ' + err.message, 'error');
        apply.disabled = false;
    }
}

function startStatusRefresh() {
    stopStatusRefresh();
    statusRefreshTimer = setInterval(loadStatus, 30000);
}

function stopStatusRefresh() {
    if (statusRefreshTimer) {
        clearInterval(statusRefreshTimer);
        statusRefreshTimer = null;
    }
}

function formatBytes(bytes) {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
}

function formatUptime(seconds) {
    const d = Math.floor(seconds / 86400);
    const h = Math.floor((seconds % 86400) / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    if (d > 0) return `${d}d ${h}h ${m}m`;
    if (h > 0) return `${h}h ${m}m`;
    return `${m}m`;
}

// ============================================
// Connectivity Tab - Tunnel
// ============================================

async function loadTunnelStatus() {
    try {
        const data = await apiGet('/tunnel/status');
        const dot = document.getElementById('tunnel-status-dot');
        const startBtn = document.getElementById('tunnel-start-btn');
        const stopBtn = document.getElementById('tunnel-stop-btn');
        const urlGroup = document.getElementById('tunnel-url-group');
        const unavailable = document.getElementById('tunnel-unavailable');

        if (!data.available) {
            dot.className = 'status-indicator status-indicator--off';
            startBtn.style.display = 'none';
            stopBtn.style.display = 'none';
            urlGroup.style.display = 'none';
            unavailable.style.display = '';
            return;
        }

        unavailable.style.display = 'none';

        if (data.running) {
            dot.className = 'status-indicator status-indicator--on';
            startBtn.style.display = 'none';
            stopBtn.style.display = '';
            if (data.url) {
                urlGroup.style.display = '';
                document.getElementById('tunnel-url').value = data.url;
            } else {
                urlGroup.style.display = 'none';
            }
        } else {
            dot.className = 'status-indicator status-indicator--off';
            startBtn.style.display = '';
            stopBtn.style.display = 'none';
            urlGroup.style.display = 'none';
        }
    } catch (err) {
        showToast('Failed to load tunnel status: ' + err.message, 'error');
    }
}

async function startTunnel() {
    try {
        await apiPost('/tunnel/start', {});
        showToast('Tunnel starting...', 'success');
        // Poll for URL to appear
        setTimeout(loadTunnelStatus, 3000);
        setTimeout(loadTunnelStatus, 8000);
        setTimeout(loadTunnelStatus, 15000);
    } catch (err) {
        showToast(err.message, 'error');
    }
}

async function stopTunnel() {
    try {
        await apiPost('/tunnel/stop', {});
        showToast('Tunnel stopped', 'success');
        loadTunnelStatus();
    } catch (err) {
        showToast(err.message, 'error');
    }
}

function copyTunnelUrl() {
    const url = document.getElementById('tunnel-url').value;
    navigator.clipboard.writeText(url).then(() => {
        showToast('Copied to clipboard', 'success');
    }).catch(() => {
        showToast('Failed to copy', 'error');
    });
}

// ============================================
// Connectivity Tab - DynDNS
// ============================================

async function loadDynDNSStatus() {
    try {
        const data = await apiGet('/dyndns/status');
        const dot = document.getElementById('dyndns-status-dot');
        const startBtn = document.getElementById('dyndns-start-btn');
        const stopBtn = document.getElementById('dyndns-stop-btn');

        if (data.running) {
            dot.className = 'status-indicator status-indicator--on';
            startBtn.style.display = 'none';
            stopBtn.style.display = '';
        } else {
            dot.className = 'status-indicator status-indicator--off';
            startBtn.style.display = '';
            stopBtn.style.display = 'none';
        }

        if (data.domain) document.getElementById('dyndns-domain').value = data.domain;
        if (data.provider) document.getElementById('dyndns-provider').value = data.provider;
        document.getElementById('dyndns-last-update').textContent = data.last_update || '-';
        document.getElementById('dyndns-last-ip').textContent = data.last_ip ? 'IP: ' + data.last_ip : '';
        document.getElementById('dyndns-last-error').textContent = data.last_error || '';

        // Also load DynDNS settings from main settings
        const settings = await apiGet('/settings');
        if (settings.dyndns) {
            document.getElementById('dyndns-provider').value = settings.dyndns.provider || 'duckdns';
            document.getElementById('dyndns-domain').value = settings.dyndns.domain || '';
            document.getElementById('dyndns-update-url').value = settings.dyndns.update_url || '';
            document.getElementById('dyndns-interval').value = settings.dyndns.interval || 300;
            // Don't populate masked token
        }
    } catch (err) {
        showToast('Failed to load DynDNS status: ' + err.message, 'error');
    }
}

// ============================================
// Audit Tab
// ============================================

// The filters, as the URL hash keeps them ("#audit?q=...&actor=..."), so a
// refresh reopens the tab exactly as it was. Dates are the From/To days as
// picked (the viewer's own days); they go to the server as instants.
const AUDIT_KEYS = ['q', 'actor', 'action', 'type', 'since', 'until', 'progress'];
const AUDIT_SEARCH_DELAY_MS = 300;
const auditState = { q: '', actor: '', action: '', type: '', since: '', until: '', progress: '' };
let auditCursor = null;
let auditSeq = 0;
let auditFacets = null;
let auditTotal = null;
let auditReady = false;
let auditSearchTimer = null;

function auditControls() {
    return {
        q: document.getElementById('audit-q'),
        actor: document.getElementById('audit-actor'),
        action: document.getElementById('audit-action'),
        type: document.getElementById('audit-type'),
        since: document.getElementById('audit-since'),
        until: document.getElementById('audit-until'),
        progress: document.getElementById('audit-progress'),
    };
}

function auditFromHash() {
    const hash = window.location.hash || '';
    if (!hash.startsWith('#audit')) return;
    const params = new URLSearchParams(hash.slice(hash.indexOf('?') + 1 || hash.length));
    AUDIT_KEYS.forEach((key) => { auditState[key] = params.get(key) || ''; });
}

function auditToHash() {
    const params = new URLSearchParams();
    AUDIT_KEYS.forEach((key) => { if (auditState[key]) params.set(key, auditState[key]); });
    const qs = params.toString();
    try {
        history.replaceState(null, '', '#audit' + (qs ? '?' + qs : ''));
    } catch (_) { /* a sandboxed frame may refuse; the filters still apply */ }
}

function auditReadControls() {
    const c = auditControls();
    auditState.q = c.q.value.trim();
    auditState.actor = c.actor.value;
    auditState.action = c.action.value;
    auditState.type = c.type.value;
    auditState.since = c.since.value;
    auditState.until = c.until.value;
    auditState.progress = c.progress.checked ? '1' : '';
}

function auditWriteControls() {
    const c = auditControls();
    c.q.value = auditState.q;
    ['actor', 'action', 'type'].forEach((key) => auditEnsureOption(c[key], auditState[key]));
    c.actor.value = auditState.actor;
    c.action.value = auditState.action;
    c.type.value = auditState.type;
    c.since.value = auditState.since;
    c.until.value = auditState.until;
    c.progress.checked = auditState.progress === '1';
}

// A value the select does not list yet (a hash from before the facets came,
// or an actor with no events now) is still the one chosen.
function auditEnsureOption(select, value) {
    if (!value || Array.from(select.options).some((o) => o.value === value)) return;
    const option = document.createElement('option');
    option.value = value;
    option.textContent = value;
    select.appendChild(option);
}

function auditFillSelect(select, values, keep) {
    const current = select.value;
    Array.from(select.options).slice(keep).forEach((o) => o.remove());
    values.forEach((value) => {
        const option = document.createElement('option');
        option.value = value;
        option.textContent = value;
        select.appendChild(option);
    });
    auditEnsureOption(select, current);
    select.value = current;
}

function initAudit() {
    if (auditReady) return;
    auditReady = true;
    auditFromHash();
    auditWriteControls();
    const c = auditControls();
    c.q.addEventListener('input', () => {
        clearTimeout(auditSearchTimer);
        auditSearchTimer = setTimeout(reloadAudit, AUDIT_SEARCH_DELAY_MS);
    });
    ['actor', 'action', 'type', 'since', 'until', 'progress'].forEach((key) => {
        c[key].addEventListener('change', reloadAudit);
    });
}

// The local day `value` (YYYY-MM-DD) starts, plus `days`, as an ISO instant.
function auditDayStart(value, days) {
    const parts = value.split('-').map(Number);
    return new Date(parts[0], parts[1] - 1, parts[2] + days).toISOString();
}

function auditQuery(cursor) {
    const params = new URLSearchParams();
    if (auditState.q) params.set('q', auditState.q);
    if (auditState.actor) params.set('actor', auditState.actor);
    if (auditState.action) params.set('action', auditState.action);
    if (auditState.type) params.set('target_type', auditState.type);
    if (auditState.since) params.set('since', auditDayStart(auditState.since, 0));
    // The To day is included: the range ends where the next day starts.
    if (auditState.until) params.set('until', auditDayStart(auditState.until, 1));
    if (auditState.progress) params.set('include_progress', '1');
    if (cursor) params.set('cursor', cursor);
    return '/audit' + (params.toString() ? '?' + params.toString() : '');
}

async function loadAudit() {
    initAudit();
    await reloadAudit();
}

// A new first page. The rows on screen stay until it arrives (no empty
// flash), and an answer to an older filter than the newest is dropped.
async function reloadAudit() {
    auditReadControls();
    auditToHash();
    const seq = ++auditSeq;
    try {
        const data = await apiGet(auditQuery(null));
        if (seq !== auditSeq) return;
        auditCursor = data.next_cursor || null;
        auditTotal = typeof data.total === 'number' ? data.total : null;
        if (data.facets) {
            auditFacets = data.facets;
            const c = auditControls();
            auditFillSelect(c.actor, data.facets.actors || [], 1);
            auditFillSelect(c.action, data.facets.actions || [], 2);
            auditFillSelect(c.type, data.facets.target_types || [], 1);
        }
        auditEvents = data.events || [];
        renderAudit();
    } catch (err) {
        if (seq !== auditSeq) return;
        auditCursor = null;
        auditBody.innerHTML = `<tr><td colspan="5" class="loading">Error: ${escapeHtml(err.message)}</td></tr>`;
        renderAuditPager();
    }
}

async function loadMoreAudit() {
    if (!auditCursor) return;
    const seq = auditSeq;
    const button = document.getElementById('audit-more');
    button.disabled = true;
    try {
        const data = await apiGet(auditQuery(auditCursor));
        if (seq !== auditSeq) return;
        auditCursor = data.next_cursor || null;
        const more = data.events || [];
        auditEvents = auditEvents.concat(more);
        auditBody.insertAdjacentHTML('beforeend', more.map(auditRowHtml).join(''));
        renderAuditPager();
    } catch (err) {
        showToast('Failed to load more events: ' + err.message, 'error');
    } finally {
        button.disabled = false;
    }
}

function auditRowHtml(event) {
    const target = event.target_path || event.target_username || '';
    return `
        <tr data-id="${event.id}">
            <td>${formatDate(event.created_at ? event.created_at.replace(' ', 'T') + 'Z' : '')}</td>
            <td>${event.actor_username ? escapeHtml(event.actor_username) : '-'}</td>
            <td>${escapeHtml(event.action)}</td>
            <td>${target ? escapeHtml(target) : '-'}</td>
            <td${event.details ? ` title="${escapeHtml(event.details)}"` : ''}>${event.details ? escapeHtml(truncate(event.details, 160)) : '-'}</td>
        </tr>`;
}

function renderAudit() {
    if (auditEvents.length === 0) {
        const empty = auditFacets && (auditFacets.actions || []).length
            ? 'No events match these filters'
            : 'No audit events yet';
        auditBody.innerHTML = `<tr><td colspan="5" class="loading">${empty}</td></tr>`;
    } else {
        auditBody.innerHTML = auditEvents.map(auditRowHtml).join('');
    }
    renderAuditPager();
}

function renderAuditPager() {
    const count = document.getElementById('audit-count');
    count.textContent = auditTotal === null
        ? ' '
        : auditTotal.toLocaleString() + (auditTotal === 1 ? ' event' : ' events') +
          (auditState.progress || auditState.type ? '' : ' · reading-progress sync hidden');
    const button = document.getElementById('audit-more');
    const more = !!auditCursor;
    button.setAttribute('aria-hidden', more ? 'false' : 'true');
    button.tabIndex = more ? 0 : -1;
}

async function saveDynDNSSettings() {
    const tokenInput = document.getElementById('dyndns-token');
    const data = {
        provider: document.getElementById('dyndns-provider').value,
        domain: document.getElementById('dyndns-domain').value,
        update_url: document.getElementById('dyndns-update-url').value,
        interval: parseInt(document.getElementById('dyndns-interval').value, 10),
    };
    // Only send token if user typed something
    if (tokenInput.value) {
        data.token = tokenInput.value;
    }
    try {
        await apiPut('/settings/dyndns', data);
        showToast('DynDNS settings saved', 'success');
        tokenInput.value = '';
    } catch (err) {
        showToast(err.message, 'error');
    }
}

async function startDynDNS() {
    try {
        await apiPost('/dyndns/start', {});
        showToast('DynDNS started', 'success');
        loadDynDNSStatus();
    } catch (err) {
        showToast(err.message, 'error');
    }
}

async function stopDynDNS() {
    try {
        await apiPost('/dyndns/stop', {});
        showToast('DynDNS stopped', 'success');
        loadDynDNSStatus();
    } catch (err) {
        showToast(err.message, 'error');
    }
}

async function testDynDNS() {
    try {
        const result = await apiPost('/dyndns/test', {});
        if (result.success) {
            showToast('DNS update successful: ' + (result.ip || ''), 'success');
        } else {
            showToast('DNS update failed: ' + (result.error || 'unknown error'), 'error');
        }
        loadDynDNSStatus();
    } catch (err) {
        showToast(err.message, 'error');
    }
}

// ============================================
// Toast notifications
// ============================================

function showToast(message, type = 'info') {
    const toast = document.getElementById('toast');
    toast.textContent = message;
    toast.className = `toast toast--${type} show`;

    setTimeout(() => {
        toast.classList.remove('show');
    }, 3000);
}

// ============================================
// Utility functions
// ============================================

function escapeHtml(str) {
    if (!str) return '';
    return str
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#039;');
}

function formatDate(dateStr) {
    if (!dateStr) return '-';
    try {
        const date = new Date(dateStr);
        return date.toLocaleDateString() + ' ' + date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    } catch {
        return dateStr;
    }
}

function truncate(str, length) {
    if (!str) return '';
    if (str.length <= length) return str;
    return str.slice(0, Math.max(length - 3, 0)) + '...';
}

function getBadgeClass(status) {
    const classMap = {
        'active': 'badge--success',
        'valid': 'badge--success',
        'pending': 'badge--warning',
        'disabled': 'badge--error',
        'deleted': 'badge--muted',
        'expired': 'badge--muted',
        'used': 'badge--info',
    };
    return classMap[status] || 'badge--muted';
}
