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

// Auth helper - get Basic auth header from sessionStorage
function getAuthHeaders() {
    const auth = sessionStorage.getItem('mokuro_auth');
    if (!auth) return {};
    return { 'Authorization': 'Basic ' + auth };
}

function logout() {
    sessionStorage.removeItem('mokuro_auth');
    sessionStorage.removeItem('mokuro_user');
    window.location.href = '/';
}

// Initialize
document.addEventListener('DOMContentLoaded', () => {
    const auth = sessionStorage.getItem('mokuro_auth');
    if (!auth) {
        window.location.href = '/login';
        return;
    }
    initTabs();
    initModals();
    initForms();
    loadUsers();
    loadInvites();
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
            if (tabId === 'settings') loadSettings();
            if (tabId === 'audit') loadAudit();
            if (tabId === 'status') {
                loadStatus();
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
async function apiGet(path) {
    const response = await fetch(API_BASE + path, {
        headers: { ...getAuthHeaders() },
    });
    const data = await response.json();
    if (!response.ok) {
        throw new Error(data.error || 'Request failed');
    }
    return data;
}

async function apiPost(path, body) {
    const response = await fetch(API_BASE + path, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', ...getAuthHeaders() },
        body: JSON.stringify(body),
    });
    const data = await response.json();
    if (!response.ok) {
        throw new Error(data.error || 'Request failed');
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
        throw new Error(data.error || 'Request failed');
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
        throw new Error(data.error || 'Request failed');
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
    document.getElementById('change-role-select').value = currentRole;
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
        // OCR
        document.getElementById('settings-ocr-interval').value = data.ocr?.poll_interval || 30;
        renderOcrRuntimeStatus(data.ocr_runtime || {}, data.ocr || {});
    } catch (err) {
        showToast('Failed to load settings: ' + err.message, 'error');
    }
}

function renderOcrRuntimeStatus(runtime, ocrConfig) {
    const configured = runtime.configured_backend || ocrConfig.backend || 'auto';
    const installed = runtime.installed ? (runtime.installed_backend || 'unknown') : 'not installed';
    const envPath = runtime.env_path || '-';
    const supported = (runtime.supported_backends || []).join(', ') || 'cpu';

    document.getElementById('ocr-configured-backend').textContent = configured;
    document.getElementById('ocr-installed-backend').textContent = installed;
    document.getElementById('ocr-env-path').textContent = envPath;
    document.getElementById('ocr-supported-backends').textContent = supported;
    document.getElementById('ocr-cli-hint').textContent =
        runtime.cli_hint || 'Use `mokuro-bunko serve --ocr <auto|cuda|rocm|cpu|skip>` to change backend.';
    document.getElementById('ocr-driver-hint').textContent =
        runtime.driver_hint || '';
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

async function saveOcrSettings() {
    try {
        await apiPut('/settings/ocr', {
            poll_interval: parseInt(document.getElementById('settings-ocr-interval').value, 10),
        });
        await loadSettings();
        showToast('OCR poll interval saved', 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
}

async function saveQueueSettings() {
    try {
        await apiPut('/settings/queue', {
            show_in_nav: document.getElementById('settings-queue-show-nav').checked,
            public_access: document.getElementById('settings-queue-public').checked,
        });
        showToast('Queue settings saved', 'success');
    } catch (err) {
        showToast(err.message, 'error');
    }
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

async function loadAudit() {
    try {
        const data = await apiGet('/audit');
        auditEvents = data.events || [];
        renderAudit();
    } catch (err) {
        auditBody.innerHTML = `<tr><td colspan="5" class="loading">Error: ${err.message}</td></tr>`;
    }
}

function renderAudit() {
    if (auditEvents.length === 0) {
        auditBody.innerHTML = '<tr><td colspan="5" class="loading">No audit events yet</td></tr>';
        return;
    }

    auditBody.innerHTML = auditEvents.map((event) => `
        <tr>
            <td>${formatDate(event.created_at)}</td>
            <td>${event.actor_username ? escapeHtml(event.actor_username) : '-'}</td>
            <td>${escapeHtml(event.action)}</td>
            <td>${event.target_path ? escapeHtml(event.target_path) : (event.target_username ? escapeHtml(event.target_username) : '-')}</td>
            <td>${event.details ? escapeHtml(truncate(event.details, 120)) : '-'}</td>
        </tr>
    `).join('');
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
        'expired': 'badge--muted',
        'used': 'badge--info',
    };
    return classMap[status] || 'badge--muted';
}
