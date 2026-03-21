// Account page JavaScript

function getAuth() {
    const auth = sessionStorage.getItem('mokuro_auth');
    const user = sessionStorage.getItem('mokuro_user');
    if (!auth || !user) return null;
    try {
        return { auth, user: JSON.parse(user) };
    } catch {
        return null;
    }
}

function logout() {
    sessionStorage.removeItem('mokuro_auth');
    sessionStorage.removeItem('mokuro_user');
    window.location.href = '/';
}

async function updateHeaderNav(user) {
    if (window.renderMokuroHeaderNav) {
        await window.renderMokuroHeaderNav('account');
    }
}

// Format large numbers
function formatNumber(num) {
    if (num >= 1000000) {
        return (num / 1000000).toFixed(1).replace(/\.0$/, '') + 'M';
    }
    if (num >= 1000) {
        return (num / 1000).toFixed(1).replace(/\.0$/, '') + 'K';
    }
    return num.toLocaleString();
}

// Initialize page
document.addEventListener('DOMContentLoaded', async () => {
    const session = getAuth();
    if (!session) {
        window.location.href = '/login';
        return;
    }

    // Populate profile
    document.getElementById('profile-username').textContent = session.user.username;

    const roleBadge = document.getElementById('profile-role-badge');
    const role = session.user.role || 'registered';
    const roleClass = role === 'admin' ? 'badge--warning' : role === 'editor' ? 'badge--info' : 'badge--success';
    roleBadge.className = 'badge ' + roleClass;
    roleBadge.textContent = role;
    updateHeaderNav(session.user);

    // Set up connect section
    const serverUrl = window.location.origin;
    document.getElementById('server-url').value = serverUrl;
    const readerLink = 'https://reader.mokuro.app/#/cloud?server=' + encodeURIComponent(serverUrl) + '&username=' + encodeURIComponent(session.user.username);
    const deepLinkEl = document.getElementById('deep-link');
    deepLinkEl.href = readerLink;
    deepLinkEl.target = '_blank';
    deepLinkEl.rel = 'noopener';

    // Fetch user info for created_at
    try {
        const meResp = await fetch('/login/api/me', {
            headers: { 'Authorization': 'Basic ' + session.auth }
        });
        if (meResp.ok) {
            const meData = await meResp.json();
            if (meData.created_at) {
                const date = new Date(meData.created_at + 'Z');
                document.getElementById('profile-created').textContent = date.toLocaleDateString(undefined, {
                    year: 'numeric', month: 'long', day: 'numeric'
                });
            }
        }
    } catch {
        // Leave as dash
    }

    // Load stats
    loadStats(session.auth);

    // Set up password form
    document.getElementById('password-form').addEventListener('submit', (e) => handlePasswordChange(e, session));

    const copyBtn = document.getElementById('copy-btn');
    if (copyBtn) {
        copyBtn.addEventListener('click', copyServerUrl);
    }

    const showDeleteBtn = document.getElementById('show-delete-modal-btn');
    if (showDeleteBtn) {
        showDeleteBtn.addEventListener('click', showDeleteModal);
    }

    const hideDeleteBtn = document.getElementById('hide-delete-modal-btn');
    if (hideDeleteBtn) {
        hideDeleteBtn.addEventListener('click', hideDeleteModal);
    }

    const cancelDeleteBtn = document.getElementById('cancel-delete-btn');
    if (cancelDeleteBtn) {
        cancelDeleteBtn.addEventListener('click', hideDeleteModal);
    }

    const deleteConfirmBtn = document.getElementById('delete-confirm-btn');
    if (deleteConfirmBtn) {
        deleteConfirmBtn.addEventListener('click', deleteAccount);
    }
});

async function loadStats(auth) {
    try {
        const response = await fetch('/api/account/stats', {
            headers: { 'Authorization': 'Basic ' + auth }
        });
        if (!response.ok) throw new Error('Failed to fetch stats');
        const data = await response.json();

        document.getElementById('stat-volumes').textContent = formatNumber(data.volumes || 0);
        document.getElementById('stat-pages').textContent = formatNumber(data.pages_read || 0);
        document.getElementById('stat-characters').textContent = formatNumber(data.characters_read || 0);
        document.getElementById('stat-time').textContent = data.reading_time_formatted || '0s';
    } catch {
        // Leave dashes on error
    }
}

async function handlePasswordChange(e, session) {
    e.preventDefault();

    const currentPassword = document.getElementById('current-password').value;
    const newPassword = document.getElementById('new-password').value;
    const confirmPassword = document.getElementById('confirm-password').value;
    const successEl = document.getElementById('password-success');
    const errorEl = document.getElementById('password-error');
    const btn = document.getElementById('password-btn');

    successEl.style.display = 'none';
    errorEl.style.display = 'none';

    if (newPassword !== confirmPassword) {
        errorEl.textContent = 'New passwords do not match.';
        errorEl.style.display = 'block';
        return;
    }

    if (newPassword.length < 8) {
        errorEl.textContent = 'Password must be at least 8 characters.';
        errorEl.style.display = 'block';
        return;
    }

    btn.disabled = true;
    btn.textContent = 'Updating...';

    try {
        const response = await fetch('/api/account/password', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
                'Authorization': 'Basic ' + session.auth
            },
            body: JSON.stringify({
                current_password: currentPassword,
                new_password: newPassword
            })
        });

        const data = await response.json();

        if (!response.ok) {
            throw new Error(data.error || 'Failed to change password');
        }

        // Update stored credentials
        const newAuth = btoa(session.user.username + ':' + newPassword);
        sessionStorage.setItem('mokuro_auth', newAuth);
        session.auth = newAuth;

        // Clear form and show success
        document.getElementById('password-form').reset();
        successEl.style.display = 'block';
    } catch (err) {
        errorEl.textContent = err.message;
        errorEl.style.display = 'block';
    } finally {
        btn.disabled = false;
        btn.textContent = 'Update Password';
    }
}

function copyServerUrl() {
    const input = document.getElementById('server-url');
    const btn = document.getElementById('copy-btn');
    navigator.clipboard.writeText(input.value).then(() => {
        btn.textContent = 'Copied!';
        setTimeout(() => { btn.textContent = 'Copy'; }, 2000);
    });
}

function showDeleteModal() {
    document.getElementById('delete-modal').classList.add('open');
    document.getElementById('delete-password').value = '';
    document.getElementById('delete-error').style.display = 'none';
}

function hideDeleteModal() {
    document.getElementById('delete-modal').classList.remove('open');
}

async function deleteAccount() {
    const session = getAuth();
    if (!session) return;

    const password = document.getElementById('delete-password').value;
    const errorEl = document.getElementById('delete-error');
    const btn = document.getElementById('delete-confirm-btn');

    if (!password) {
        errorEl.textContent = 'Password is required.';
        errorEl.style.display = 'block';
        return;
    }

    errorEl.style.display = 'none';
    btn.disabled = true;
    btn.textContent = 'Deleting...';

    try {
        const response = await fetch('/api/account/delete', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
                'Authorization': 'Basic ' + session.auth
            },
            body: JSON.stringify({ password })
        });

        const data = await response.json();

        if (!response.ok) {
            throw new Error(data.error || 'Failed to delete account');
        }

        sessionStorage.removeItem('mokuro_auth');
        sessionStorage.removeItem('mokuro_user');
        window.location.href = '/';
    } catch (err) {
        errorEl.textContent = err.message;
        errorEl.style.display = 'block';
        btn.disabled = false;
        btn.textContent = 'Delete Account';
    }
}
