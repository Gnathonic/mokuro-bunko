// Catalog JavaScript

const API_BASE = '/catalog/api';

// State
let series = [];
let filtered = [];
let currentView = 'root'; // 'root' or 'series'
let currentSeries = null;
let currentVolumes = [];
let filteredVolumes = [];
let readerUrl = 'https://reader.mokuro.app';
let serverReaderUrl = 'https://reader.mokuro.app';
let ocrStatus = { active: false };
let ocrStatusTimer = null;
let ocrEtaTickTimer = null;
let lastOcrStatusReceivedAtMs = 0;

// DOM
const grid = document.getElementById('catalog-grid');
const empty = document.getElementById('catalog-empty');
const search = document.getElementById('search');
const breadcrumb = document.getElementById('breadcrumb');
const headerNav = document.getElementById('header-nav');

document.addEventListener('DOMContentLoaded', () => {
    updateNav();
    initTitleLang();
    initSortMode();
    loadCatalog();
    startOcrStatusPolling();
    startEtaTicker();
    search.addEventListener('input', () => {
        const query = search.value.toLowerCase().trim();
        if (currentView === 'root') {
            filterSeries(query);
        } else {
            filterVolumes(query);
        }
    });

    // Handle browser back/forward
    window.addEventListener('popstate', (e) => {
        if (e.state && e.state.series) {
            openSeries(e.state.series, true);
        } else {
            const hashSeries = getSeriesFromHash();
            if (hashSeries) {
                openSeries(hashSeries, true);
            } else {
                showRoot(true);
            }
        }
    });

    // Handle direct hash navigation (including pasted URLs).
    window.addEventListener('hashchange', () => {
        const hashSeries = getSeriesFromHash();
        if (!hashSeries) {
            showRoot(true);
            return;
        }
        if (currentView === 'series' && currentSeries && currentSeries.name === hashSeries) {
            return;
        }
        openSeries(hashSeries, true);
    });
});

// --- Series title language -------------------------------------------------

const TITLE_PREF_KEY = 'mokuro_catalog_title_lang';
// Each option is a progression, not a single language: fall through the chain,
// ending at the folder name. Default is the Native progression.
const TITLE_PROGRESSIONS = {
    native: ['native', 'romaji', 'english'],
    english: ['english', 'romaji', 'native'],
    folder: [],
};
const titleCollator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

function getTitlePref() {
    const value = localStorage.getItem(TITLE_PREF_KEY);
    return Object.prototype.hasOwnProperty.call(TITLE_PROGRESSIONS, value) ? value : 'native';
}

function displayTitle(s) {
    const chain = TITLE_PROGRESSIONS[getTitlePref()];
    if (s.titles) {
        for (const lang of chain) {
            if (s.titles[lang]) return s.titles[lang];
        }
    }
    return s.name;
}

function displayTitleForName(name) {
    const entry = series.find(s => s.name === name);
    return entry ? displayTitle(entry) : name;
}

// --- Genre filter -----------------------------------------------------------

let activeGenre = null;

function seriesGenres(s) {
    return (s.community && Array.isArray(s.community.genres)) ? s.community.genres : [];
}

function renderGenreChips() {
    const host = document.getElementById('genre-chips');
    if (!host) return;
    const counts = new Map();
    series.forEach(s => seriesGenres(s).forEach(g => counts.set(g, (counts.get(g) || 0) + 1)));
    if (counts.size === 0) {
        host.innerHTML = '';
        host.style.display = 'none';
        return;
    }
    host.style.display = '';
    const genres = [...counts.keys()].sort((a, b) => (counts.get(b) - counts.get(a)) || a.localeCompare(b));
    host.innerHTML = genres.map(g => {
        const active = g === activeGenre ? ' genre-chip--active' : '';
        return '<button class="genre-chip' + active + '" data-genre="' + escapeAttr(g) + '">'
            + escapeHtml(g) + ' <span class="genre-chip__count">' + counts.get(g) + '</span></button>';
    }).join('');
    host.querySelectorAll('.genre-chip').forEach(chip => {
        chip.addEventListener('click', () => {
            const genre = chip.dataset.genre;
            activeGenre = activeGenre === genre ? null : genre;
            renderGenreChips();
            filterSeries(search.value.toLowerCase().trim());
        });
    });
}

// --- Sorting ----------------------------------------------------------------

const SORT_PREF_KEY = 'mokuro_catalog_sort';
const SORT_MODES = ['title', 'newest', 'densest', 'rating'];

function getSortPref() {
    const value = localStorage.getItem(SORT_PREF_KEY);
    return SORT_MODES.includes(value) ? value : 'title';
}

function seriesDensity(s) {
    if (!s.total_pages || !s.total_chars) return 0;
    return s.total_chars / s.total_pages;
}

function sortSeries() {
    const byTitle = (a, b) => titleCollator.compare(displayTitle(a), displayTitle(b));
    const mode = getSortPref();
    if (mode === 'newest') {
        series.sort((a, b) =>
            ((b.latest_volume_modified || 0) - (a.latest_volume_modified || 0)) || byTitle(a, b));
    } else if (mode === 'rating') {
        const score = s => (s.community && typeof s.community.score === 'number') ? s.community.score : -1;
        series.sort((a, b) => (score(b) - score(a)) || byTitle(a, b));
    } else if (mode === 'densest') {
        series.sort((a, b) => (seriesDensity(b) - seriesDensity(a)) || byTitle(a, b));
    } else {
        series.sort(byTitle);
    }
}

function initTitleLang() {
    const select = document.getElementById('title-lang');
    if (!select) return;
    select.value = getTitlePref();
    select.addEventListener('change', () => {
        try {
            localStorage.setItem(TITLE_PREF_KEY, select.value);
        } catch (_) { /* private mode: preference just won't persist */ }
        sortSeries();
        if (currentView === 'root') {
            filterSeries(search.value.toLowerCase().trim());
        } else {
            renderBreadcrumb();
        }
    });
}

function initSortMode() {
    const select = document.getElementById('sort-mode');
    if (!select) return;
    select.value = getSortPref();
    select.addEventListener('change', () => {
        try {
            localStorage.setItem(SORT_PREF_KEY, select.value);
        } catch (_) { /* private mode: preference just won't persist */ }
        sortSeries();
        if (currentView === 'root') {
            filterSeries(search.value.toLowerCase().trim());
        }
    });
}

function getSessionUser() {
    const userStr = sessionStorage.getItem('mokuro_user');
    if (!userStr) return null;
    try {
        return JSON.parse(userStr);
    } catch (_) {
        return null;
    }
}

async function updateNav() {
    if (!headerNav) return;
    if (window.renderMokuroHeaderNav) {
        await window.renderMokuroHeaderNav('catalog');
    }
}

function logout() {
    sessionStorage.removeItem('mokuro_auth');
    sessionStorage.removeItem('mokuro_user');
    window.location.href = '/';
}

// Load root catalog
async function loadCatalog() {
    try {
        // Fetch config and library in parallel
        const [configRes, libraryRes, ocrRes] = await Promise.all([
            fetch(API_BASE + '/config'),
            fetch(API_BASE + '/library'),
            fetch(API_BASE + '/ocr-status'),
        ]);
        const config = await configRes.json();
        const data = await libraryRes.json();
        ocrStatus = ocrRes.ok ? await ocrRes.json() : { active: false };
        lastOcrStatusReceivedAtMs = Date.now();
        serverReaderUrl = config.reader_url || serverReaderUrl;
        // User override from localStorage takes priority
        const userOverride = localStorage.getItem('mokuro_reader_url');
        readerUrl = userOverride || serverReaderUrl;
        series = data.series || [];
        sortSeries();
        filtered = series;
        renderGenreChips();
        const hashSeries = getSeriesFromHash();
        if (hashSeries) {
            await openSeries(hashSeries, true);
        } else {
            renderRoot();
        }
        initReaderSettings();
    } catch (err) {
        grid.innerHTML = '<div class="loading">Error loading catalog: ' + escapeHtml(err.message) + '</div>';
    }
}

function startOcrStatusPolling() {
    if (ocrStatusTimer) clearInterval(ocrStatusTimer);
    ocrStatusTimer = setInterval(refreshOcrStatus, 2000);
}

function startEtaTicker() {
    if (ocrEtaTickTimer) clearInterval(ocrEtaTickTimer);
    ocrEtaTickTimer = setInterval(() => {
        if (currentView === 'series' && ocrStatus && ocrStatus.active) {
            renderVolumes();
        }
    }, 1000);
}

async function refreshOcrStatus() {
    try {
        const response = await fetch(API_BASE + '/ocr-status');
        if (!response.ok) return;
        ocrStatus = await response.json();
        lastOcrStatusReceivedAtMs = Date.now();
        if (currentView === 'series') {
            renderVolumes();
        }
    } catch (_) {
        // Ignore transient polling errors.
    }
}

function getSeriesFromHash() {
    if (!window.location.hash || window.location.hash.length < 2) return null;
    const raw = window.location.hash.slice(1);
    try {
        return decodeURIComponent(raw);
    } catch (_) {
        // Fallback for malformed hashes: treat as literal.
        return raw;
    }
}

// Filter series (root view) — matches the folder name and every known title
function filterSeries(query) {
    const matchesQuery = s => {
        if (!query) return true;
        if (s.name.toLowerCase().includes(query)) return true;
        if (s.titles && Object.values(s.titles).some(t => String(t).toLowerCase().includes(query))) {
            return true;
        }
        return seriesGenres(s).some(g => g.toLowerCase().includes(query));
    };
    const matchesGenre = s => !activeGenre || seriesGenres(s).includes(activeGenre);
    filtered = series.filter(s => matchesQuery(s) && matchesGenre(s));
    renderRoot();
}

// Filter volumes (series view)
function filterVolumes(query) {
    if (!query) {
        filteredVolumes = currentVolumes;
    } else {
        filteredVolumes = currentVolumes.filter(v => v.name.toLowerCase().includes(query));
    }
    renderVolumes();
}

// Show root view
function showRoot(fromPopState) {
    currentView = 'root';
    currentSeries = null;
    search.value = '';
    search.placeholder = 'Search library...';
    filtered = series;
    renderBreadcrumb();
    renderRoot();
    if (!fromPopState) {
        history.pushState(null, '', '/catalog');
    }
}

// Open a series
async function openSeries(seriesName, fromPopState) {
    grid.innerHTML = '<div class="loading">Loading...</div>';
    empty.style.display = 'none';

    try {
        const response = await fetch(API_BASE + '/series?name=' + encodeURIComponent(seriesName));
        if (!response.ok) {
            throw new Error('Series not found');
        }
        const data = await response.json();

        currentView = 'series';
        currentSeries = data;
        currentVolumes = data.volumes || [];
        filteredVolumes = currentVolumes;
        search.value = '';
        search.placeholder = 'Search volumes...';
        renderBreadcrumb();
        renderVolumes();

        if (!fromPopState) {
            history.pushState({ series: seriesName }, '', '/catalog#' + encodeURIComponent(seriesName));
        }
    } catch (err) {
        grid.innerHTML = '<div class="loading">Error: ' + escapeHtml(err.message) + '</div>';
    }
}

// Breadcrumb
function renderBreadcrumb() {
    if (currentView === 'root') {
        breadcrumb.innerHTML =
            '<span class="catalog-breadcrumb__item catalog-breadcrumb__item--active">Catalog</span>';
    } else {
        breadcrumb.innerHTML =
            '<a href="#" class="catalog-breadcrumb__item catalog-breadcrumb__link" onclick="showRoot(); return false;">Catalog</a>' +
            '<span class="catalog-breadcrumb__sep">/</span>' +
            '<span class="catalog-breadcrumb__item catalog-breadcrumb__item--active">' + escapeHtml(displayTitleForName(currentSeries.name)) + '</span>';
    }
}

// Render root (series grid)
function renderRoot() {
    if (filtered.length === 0) {
        grid.innerHTML = '';
        empty.style.display = 'block';
        return;
    }

    empty.style.display = 'none';
    grid.innerHTML = filtered.map(s => {
        const volumeCount = s.volume_count || 0;
        const hasMultiple = volumeCount > 1;
        const coverUrl = s.cover ? (API_BASE + '/cover?path=' + encodeURIComponent(s.cover)) : null;
        const stackedClass = hasMultiple ? 'volume-card__cover--stacked' : '';
        const title = displayTitle(s);
        const score = (s.community && typeof s.community.score === 'number') ? s.community.score : null;
        const scoreBadge = score !== null
            ? ' · ★ ' + (Math.round(score) / 10).toFixed(1)
            : '';

        return '<div class="volume-card" onclick="openSeries(\'' + escapeAttr(s.name) + '\')">' +
            '<div class="volume-card__cover ' + stackedClass + '">' + coverImg(coverUrl, title) + '</div>' +
            '<div class="volume-card__info">' +
            '<div class="volume-card__title">' + escapeHtml(title) + '</div>' +
            '<div class="volume-card__count">' + volumeCount + ' volume' + (volumeCount !== 1 ? 's' : '') + scoreBadge + '</div>' +
            '</div></div>';
    }).join('');
}

// Open a volume in mokuro-reader
function openVolume(seriesName, volumeName, coverPath) {
    const cbzPath = '/mokuro-reader/' + encodeURIComponent(seriesName) + '/' + encodeURIComponent(volumeName) + '.cbz';
    const cbzUrl = new URL(cbzPath, window.location.origin).toString();
    const params = new URLSearchParams({ cbz: cbzUrl });
    if (coverPath) {
        // Optional hint; reader now assumes sidecars by default from cbz stem.
        params.set('cover', coverPath);
    }
    const url = readerUrl + '/#/upload?' + params.toString();
    window.open(url, '_blank');
}

// Render volumes (series detail)
function renderVolumes() {
    if (filteredVolumes.length === 0) {
        grid.innerHTML = '';
        empty.style.display = 'block';
        return;
    }

    empty.style.display = 'none';
    const seriesName = currentSeries.name;
    grid.innerHTML = filteredVolumes.map(v => {
        const coverUrl = v.cover ? (API_BASE + '/cover?path=' + encodeURIComponent(v.cover)) : null;
        const coverArg = v.cover ? ', \'' + escapeAttr(v.cover) + '\'' : '';
        const isActiveVolume =
            ocrStatus &&
            ocrStatus.active &&
            ocrStatus.series === seriesName &&
            ocrStatus.volume === v.name;
        const pendingBadge = (v.ocr_pending && !isActiveVolume)
            ? '<span class="volume-card__badge">OCR pending</span>'
            : '';
        const progressBadge = isActiveVolume
            ? (() => {
                const liveEta = getLiveEtaSeconds();
                const etaText = liveEta === null ? 'estimating...' : formatEta(liveEta);
                return '<span class="volume-card__badge volume-card__badge--progress">OCR '
                    + (typeof ocrStatus.percent === 'number' ? (ocrStatus.percent + '%') : 'running')
                    + ' ETA '
                    + etaText
                    + '</span>';
            })()
            : '';

        return '<div class="volume-card" onclick="openVolume(\'' + escapeAttr(seriesName) + '\', \'' + escapeAttr(v.name) + '\'' + coverArg + ')">' +
            '<div class="volume-card__cover">' + coverImg(coverUrl, v.name) + '</div>' +
            '<div class="volume-card__info">' +
            '<div class="volume-card__title">' + escapeHtml(v.name) + '</div>' +
            progressBadge + pendingBadge +
            '</div></div>';
    }).join('');
}

function formatEta(seconds) {
    const safe = Math.max(0, Math.floor(seconds));
    const hours = Math.floor(safe / 3600);
    const mins = Math.floor((safe % 3600) / 60);
    const secs = safe % 60;
    return hours + ':' + mins + ':' + String(secs).padStart(2, '0');
}

function getLiveEtaSeconds() {
    if (!ocrStatus || typeof ocrStatus.eta_seconds !== 'number' || ocrStatus.eta_seconds < 0) {
        return null;
    }
    const updatedAtSec = typeof ocrStatus.updated_at === 'number'
        ? ocrStatus.updated_at
        : (lastOcrStatusReceivedAtMs / 1000);
    const elapsed = Math.max(0, Math.floor(Date.now() / 1000 - updatedAtSec));
    return Math.max(0, ocrStatus.eta_seconds - elapsed);
}

// Cover image or placeholder
function coverImg(url, alt) {
    if (url) {
        return '<img src="' + url + '" alt="' + escapeAttr(alt) + '" loading="lazy">';
    }
    return '<div class="volume-card__placeholder"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"/></svg></div>';
}

// Reader URL settings popover
function initReaderSettings() {
    const toolbar = document.querySelector('.catalog-toolbar');
    if (!toolbar || document.getElementById('reader-settings-btn')) return;

    const btn = document.createElement('button');
    btn.id = 'reader-settings-btn';
    btn.className = 'reader-settings-btn';
    btn.title = 'Reader settings';
    btn.innerHTML = '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>';
    btn.addEventListener('click', toggleReaderPopover);
    toolbar.appendChild(btn);
}

function toggleReaderPopover() {
    let popover = document.getElementById('reader-popover');
    if (popover) {
        popover.remove();
        return;
    }

    const btn = document.getElementById('reader-settings-btn');
    popover = document.createElement('div');
    popover.id = 'reader-popover';
    popover.className = 'reader-popover';

    const userOverride = localStorage.getItem('mokuro_reader_url');
    const isDefault = !userOverride;

    popover.innerHTML =
        '<div class="reader-popover__title">Reader URL</div>' +
        '<label class="reader-popover__radio"><input type="radio" name="user-reader" value="default"' + (isDefault ? ' checked' : '') + '> Server default (' + escapeHtml(serverReaderUrl) + ')</label>' +
        '<label class="reader-popover__radio"><input type="radio" name="user-reader" value="custom"' + (!isDefault ? ' checked' : '') + '> Custom</label>' +
        '<input type="text" id="user-reader-input" class="reader-popover__input" placeholder="https://my-reader.example.com" value="' + escapeHtml(userOverride || '') + '"' + (isDefault ? ' style="display:none"' : '') + '>' +
        '<div class="reader-popover__actions">' +
        '<button class="btn btn--primary btn--sm" onclick="saveUserReaderUrl()">Save</button>' +
        '</div>';

    btn.parentElement.style.position = 'relative';
    btn.parentElement.appendChild(popover);

    // Toggle custom input visibility
    popover.querySelectorAll('input[name="user-reader"]').forEach(function(radio) {
        radio.addEventListener('change', function() {
            document.getElementById('user-reader-input').style.display = this.value === 'custom' ? '' : 'none';
        });
    });

    // Close on outside click
    setTimeout(function() {
        document.addEventListener('click', closePopoverOnOutside);
    }, 0);
}

function closePopoverOnOutside(e) {
    const popover = document.getElementById('reader-popover');
    const btn = document.getElementById('reader-settings-btn');
    if (popover && !popover.contains(e.target) && e.target !== btn && !btn.contains(e.target)) {
        popover.remove();
        document.removeEventListener('click', closePopoverOnOutside);
    }
}

function saveUserReaderUrl() {
    const selected = document.querySelector('input[name="user-reader"]:checked');
    if (selected && selected.value === 'custom') {
        const url = document.getElementById('user-reader-input').value.trim().replace(/\/+$/, '');
        if (url) {
            localStorage.setItem('mokuro_reader_url', url);
            readerUrl = url;
        }
    } else {
        localStorage.removeItem('mokuro_reader_url');
        readerUrl = serverReaderUrl;
    }
    var popover = document.getElementById('reader-popover');
    if (popover) popover.remove();
    document.removeEventListener('click', closePopoverOnOutside);
}

function escapeHtml(str) {
    if (!str) return '';
    return str
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#039;');
}

function escapeAttr(str) {
    if (!str) return '';
    return str
        .replace(/\\/g, '\\\\')
        .replace(/'/g, "\\'")
        .replace(/"/g, '&quot;');
}
