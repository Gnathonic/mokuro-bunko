/**
 * Home page JavaScript for Mokuro Bunko
 */

// Format large numbers with K/M suffixes
function formatNumber(num) {
    if (num >= 1000000) {
        return (num / 1000000).toFixed(1).replace(/\.0$/, '') + 'M';
    }
    if (num >= 1000) {
        return (num / 1000).toFixed(1).replace(/\.0$/, '') + 'K';
    }
    return num.toLocaleString();
}

function formatDuration(seconds) {
    if (!Number.isFinite(seconds) || seconds < 0) {
        return '-';
    }
    const d = Math.floor(seconds / 86400);
    const h = Math.floor((seconds % 86400) / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    const s = Math.floor(seconds % 60);
    const parts = [];
    if (d) parts.push(d + 'd');
    if (h) parts.push(h + 'h');
    if (m) parts.push(m + 'm');
    if (!parts.length || s) parts.push(s + 's');
    return parts.join(' ');
}

// Animate counter from 0 to target value
function animateValue(element, target, duration = 1000) {
    const start = 0;
    const startTime = performance.now();

    function update(currentTime) {
        const elapsed = currentTime - startTime;
        const progress = Math.min(elapsed / duration, 1);

        // Ease out cubic
        const easeProgress = 1 - Math.pow(1 - progress, 3);
        const current = Math.floor(start + (target - start) * easeProgress);

        element.textContent = formatNumber(current);

        if (progress < 1) {
            requestAnimationFrame(update);
        } else {
            element.textContent = formatNumber(target);
        }
    }

    requestAnimationFrame(update);
}

// Load stats from API
async function loadStats() {
    const statsElements = {
        users: document.getElementById('stat-users'),
        volumes: document.getElementById('stat-volumes'),
        pages: document.getElementById('stat-pages'),
        characters: document.getElementById('stat-characters'),
        time: document.getElementById('stat-time'),
    };

    // Set loading state
    Object.values(statsElements).forEach(el => {
        if (el) el.classList.add('loading');
    });

    try {
        const response = await fetch('/api/stats');
        if (!response.ok) {
            throw new Error('Failed to fetch stats');
        }

        const data = await response.json();

        // Remove loading state
        Object.values(statsElements).forEach(el => {
            if (el) el.classList.remove('loading');
        });

        // Animate the values
        if (statsElements.users) {
            animateValue(statsElements.users, data.total_users || 0);
        }
        if (statsElements.volumes) {
            animateValue(statsElements.volumes, data.total_volumes || 0);
        }
        if (statsElements.pages) {
            animateValue(statsElements.pages, data.total_pages_read || 0);
        }
        if (statsElements.characters) {
            animateValue(statsElements.characters, data.total_characters_read || 0);
        }
        if (statsElements.time) {
            // Time is already formatted from the API
            statsElements.time.textContent = data.total_reading_time_formatted || '0s';
        }

    } catch (error) {
        console.error('Failed to load stats:', error);

        // Show dashes on error
        Object.values(statsElements).forEach(el => {
            if (el) {
                el.classList.remove('loading');
                el.textContent = '-';
            }
        });
    }
}

async function loadHealth() {
    const $status = document.getElementById('health-status');
    const $db = document.getElementById('health-db');
    const $library = document.getElementById('health-library');
    const $uptime = document.getElementById('health-uptime');
    const $stamp = document.getElementById('health-last-updated');

    if (!$status || !$db || !$library || !$uptime) {
        return;
    }

    try {
        const response = await fetch('/api/health');
        if (!response.ok) {
            throw new Error('Failed to fetch health status');
        }
        const data = await response.json();
        $status.textContent = data.status || 'unknown';
        $db.textContent = data.db_status || 'unknown';
        $library.textContent = data.library_status || 'unknown';
        $uptime.textContent = formatDuration(data.uptime_seconds || 0);

        $status.className = 'health-card__value health-card__value--' + ((data.status || 'unknown').toLowerCase());
        $db.className = 'health-card__value health-card__value--' + ((data.db_status || 'unknown').toLowerCase());
        $library.className = 'health-card__value health-card__value--' + ((data.library_status || 'unknown').toLowerCase());

        if ($stamp) {
            $stamp.textContent = 'Updated ' + new Date().toLocaleTimeString();
        }
    } catch (error) {
        console.error('Failed to load health:', error);
        $status.textContent = 'unavailable';
        $db.textContent = 'unavailable';
        $library.textContent = 'unavailable';
        $uptime.textContent = '-';
    }
}

// Update header nav based on auth state
async function updateNav() {
    if (window.renderMokuroHeaderNav) {
        await window.renderMokuroHeaderNav('home');
    }
}

function logout() {
    sessionStorage.removeItem('mokuro_auth');
    sessionStorage.removeItem('mokuro_user');
    window.location.reload();
}

function showToast(message) {
    const $toast = document.getElementById('home-toast');
    if (!$toast) return;
    $toast.textContent = message;
    $toast.className = 'home-toast home-toast--show';
    clearTimeout(showToast._timer);
    showToast._timer = setTimeout(() => {
        $toast.className = 'home-toast';
    }, 2200);
}

async function copyCurrentOrigin() {
    try {
        await navigator.clipboard.writeText(window.location.origin);
        showToast('Server URL copied to clipboard');
    } catch (_err) {
        showToast('Could not copy URL from this browser context');
    }
}

// Initialize when DOM is ready
document.addEventListener('DOMContentLoaded', () => {
    updateNav();
    loadStats();
    loadHealth();

    const $copyServerUrl = document.getElementById('copy-server-url');
    if ($copyServerUrl) {
        $copyServerUrl.addEventListener('click', copyCurrentOrigin);
    }

    setInterval(loadHealth, 10000);
});
