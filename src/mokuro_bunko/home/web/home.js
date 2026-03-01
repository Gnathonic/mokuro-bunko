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

// Initialize when DOM is ready
document.addEventListener('DOMContentLoaded', () => {
    updateNav();
    loadStats();
});
