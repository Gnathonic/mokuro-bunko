// Analytics tab: how the server is used, from `/_admin/api/analytics/*`.
// WebDAV is first-class here, as it is in the accounting: every traffic,
// latency and error view can be read for WebDAV alone, and WebDAV gets its
// own by-method breakdown. Uses admin.js's apiGet/apiPost/apiPut (relative
// to API_BASE, with the tab's token) and analytics-charts.js for figures.
(function () {
    'use strict';

    const C = window.BunkoCharts;
    const { h, clear } = C;

    const RANGES = [
        { key: '24h', label: '24h', long: 'the last 24 hours' },
        { key: '7d', label: '7d', long: 'the last 7 days' },
        { key: '30d', label: '30d', long: 'the last 30 days' },
        { key: '90d', label: '90d', long: 'the last 90 days' },
        { key: '365d', label: '365d', long: 'the last 365 days' },
        { key: 'all', label: 'All', long: 'all time' },
    ];
    const HOST_RANGES = ['24h', '7d', '30d'];

    const VIEWS = [
        { id: 'overview', label: 'Overview' },
        { id: 'traffic', label: 'Traffic' },
        { id: 'popular', label: 'Popular' },
        { id: 'reading', label: 'Reading' },
        { id: 'users', label: 'Users' },
        { id: 'library', label: 'Library' },
        { id: 'clients', label: 'Clients & geography' },
        { id: 'health', label: 'Health' },
        { id: 'server', label: 'Server' },
        { id: 'crawlers', label: 'Crawlers' },
        { id: 'settings', label: 'Settings', noRange: true },
    ];

    // ------------------------------------------------------------------
    // Names for the server's keys (the server's own `label` wins)
    // ------------------------------------------------------------------

    const CATEGORY = {
        volume: 'Volumes', sidecar: 'OCR sidecars', cover: 'Covers', metadata: 'Navigation & metadata',
        progress_sync: 'Progress sync', profile_sync: 'Profile sync', personal: 'Personal layers',
        upload: 'Uploads', auth: 'Sign-in', admin: 'Admin', web: 'Web pages', processor: 'Processors', other: 'Other',
    };
    const PROTOCOL = { dav: 'WebDAV', web: 'Web pages', api: 'JSON API', processor: 'Processors' };
    const DETAIL = {
        full: 'Whole file', range: 'Range request', primary: 'Primary sidecar', layer: 'Layer sidecar',
        propfind_0: 'PROPFIND depth 0', propfind_1: 'PROPFIND depth 1', propfind_inf: 'PROPFIND depth infinity',
        options: 'OPTIONS', series_json: 'series.json', catalog_json: 'catalog.json', catalog_api: 'Catalog API',
        manifest: 'Manifests', login_me: 'Session checks', queue_poll: 'Queue polling',
        volume_data: 'volume-data.json', positions: 'positions.json', history: 'Reading history',
        profiles: 'profiles.json', goals: 'goals.json', archive: 'Archives', folder: 'Folders', '': '(no detail)',
    };
    const APP = {
        'reader-hosted': 'Mokuro Reader (hosted)', 'reader-other': 'Mokuro Reader (other origin)',
        'bunko-web': 'Bunko web pages', 'webdav-client': 'WebDAV clients', 'api-client': 'API clients', unknown: 'Unknown',
    };
    const CLIENT = {
        browser: 'Browser', rclone: 'rclone', davfs2: 'davfs2', 'windows-webdav': 'Windows WebDAV',
        'macos-finder': 'macOS Finder', cyberduck: 'Cyberduck', gvfs: 'GNOME Files (gvfs)', kodi: 'Kodi',
        nextcloud: 'Nextcloud', winscp: 'WinSCP', curl: 'curl', wget: 'wget', python: 'Python', go: 'Go',
        node: 'Node.js', bot: 'Crawler bots', other: 'Other', none: 'No user agent',
    };
    const BROWSER = { firefox: 'Firefox', chrome: 'Chrome', edge: 'Edge', safari: 'Safari', opera: 'Opera', samsung: 'Samsung Internet', other: 'Other', '-': 'Not a browser' };
    const OS = { windows: 'Windows', macos: 'macOS', ios: 'iOS', android: 'Android', linux: 'Linux', chromeos: 'ChromeOS', other: 'Other', '-': 'Unknown' };
    const KIND = { user: 'Account', anon: 'Anonymous', processor: 'Processor' };
    const LIB_KIND = {
        archive: 'Archives', primary_sidecar: 'OCR sidecars', layer_sidecar: 'Layer sidecars', cover: 'Covers',
        nocover: 'No-cover markers', series_json: 'series.json', catalog_json: 'catalog.json', other: 'Other files',
    };

    function named(dict, key, serverLabel) {
        if (serverLabel) return serverLabel;
        if (key === undefined || key === null) return '-';
        return Object.prototype.hasOwnProperty.call(dict, key) ? dict[key] : String(key);
    }

    // "1 reader", "2 readers": a count and its noun.
    function many(n, word, fmt) {
        return (fmt || C.fmtInt)(n) + ' ' + (C.num(n) === 1 ? word : word + 's');
    }

    // Identity colours, fixed per entity across every view and range.
    const colors = {
        category: C.colorScale('category', { volume: 1, sidecar: 2, cover: 3, metadata: 4, progress_sync: 5, profile_sync: 6, web: 7, upload: 8, __only: true }),
        protocol: C.colorScale('protocol', { dav: 1, web: 2, api: 3, processor: 4, __only: true }),
        method: C.colorScale('method', { GET: 1, PROPFIND: 2, PUT: 3, HEAD: 4, OPTIONS: 5, DELETE: 6, POST: 7, MOVE: 8, __only: true }),
        status: C.colorScale('status', { '2xx': 'var(--an-good)', '3xx': 'var(--an-neutral)', '4xx': 'var(--an-warning)', '5xx': 'var(--an-critical)', __only: true }),
        app: C.colorScale('app', { 'reader-hosted': 1, 'reader-other': 2, 'bunko-web': 3, 'webdav-client': 4, 'api-client': 5, unknown: 'var(--an-other)' }),
        detail: C.colorScale('detail'),
        host: C.colorScale('host', { rss: 1, rss_max: 2, cpu_pct: 1, connections_max: 1, inflight_max: 2 }),
        generic: C.colorScale('generic'),
        libkind: C.colorScale('libkind', { archive: 1, primary_sidecar: 2, layer_sidecar: 3, cover: 4, nocover: 5, series_json: 6, catalog_json: 7, other: 'var(--an-other)' }),
    };

    function byColors(by) {
        if (by === 'protocol') return colors.protocol;
        if (by === 'method') return colors.method;
        if (by === 'detail') return colors.detail;
        return colors.category;
    }

    function byDict(by) {
        if (by === 'protocol') return PROTOCOL;
        if (by === 'detail') return DETAIL;
        if (by === 'method') return {};
        return CATEGORY;
    }

    let regionNames = null;
    try {
        regionNames = new Intl.DisplayNames(undefined, { type: 'region' });
    } catch (_) {
        regionNames = null;
    }
    function countryName(code) {
        if (!code) return 'Unknown';
        if (code === 'LAN') return 'Local network';
        if (code === 'ZZ' || code === '-') return 'Unknown';
        try {
            return (regionNames && regionNames.of(code)) || code;
        } catch (_) {
            return code;
        }
    }

    // ------------------------------------------------------------------
    // State
    // ------------------------------------------------------------------

    const state = {
        view: 'overview',
        range: '30d',
        seq: 0,
        ready: false,
        active: false,
        enabled: null,
        trackingSince: null,
        renderedView: null,
        traffic: { by: 'category', metric: 'bytes_out', category: '' },
        popular: { sort: 'downloads', q: '', series: '' },
        health: { protocol: '' },
        clients: { metric: 'requests' },
    };
    const els = {};

    // ------------------------------------------------------------------
    // Fetching
    // ------------------------------------------------------------------

    function qs(params) {
        const p = new URLSearchParams();
        Object.keys(params).forEach((k) => {
            if (params[k] !== undefined && params[k] !== null && params[k] !== '') p.set(k, params[k]);
        });
        const text = p.toString();
        return text ? '?' + text : '';
    }

    // GET one analytics endpoint for the current range. Every answer carries
    // the common fields; the banner and the "tracking since" line follow it.
    async function get(endpoint, params) {
        const data = await apiGet('/analytics/' + endpoint + qs(Object.assign({ range: state.range }, params || {})));
        noteCommon(data);
        return data;
    }

    // Like get(), but a failure becomes a value a panel can show in place.
    async function tryGet(endpoint, params) {
        try {
            return { data: await get(endpoint, params) };
        } catch (err) {
            return { error: err };
        }
    }

    function noteCommon(data) {
        if (!data || typeof data !== 'object') return;
        if (typeof data.enabled === 'boolean') state.enabled = data.enabled;
        if (data.tracking_since !== undefined) state.trackingSince = data.tracking_since;
        drawBanner();
        drawSince();
    }

    // ------------------------------------------------------------------
    // Chrome: range picker, view nav, banner, hash
    // ------------------------------------------------------------------

    function build() {
        const root = document.getElementById('analytics-tab');
        if (!root || state.ready) return;
        state.ready = true;
        els.root = root;
        els.since = document.getElementById('an-since');
        els.range = document.getElementById('an-range');
        els.views = document.getElementById('an-views');
        els.banner = document.getElementById('an-banner');
        els.controls = document.getElementById('an-controls');
        els.body = document.getElementById('an-body');
        document.getElementById('an-refresh').addEventListener('click', () => render());

        RANGES.forEach((r) => {
            const btn = h('button', {
                type: 'button',
                class: 'an-range__btn',
                role: 'radio',
                'aria-checked': 'false',
                'data-range': r.key,
                title: 'Show ' + r.long,
                text: r.label,
                onclick: () => setRange(r.key),
            });
            els.range.appendChild(btn);
        });
        // Arrow keys move within the radio group.
        els.range.addEventListener('keydown', (e) => {
            if (!['ArrowLeft', 'ArrowRight'].includes(e.key)) return;
            const keys = RANGES.map((r) => r.key);
            const i = keys.indexOf(state.range) + (e.key === 'ArrowRight' ? 1 : -1);
            if (i < 0 || i >= keys.length) return;
            e.preventDefault();
            setRange(keys[i]);
            const next = els.range.querySelector('[data-range="' + keys[i] + '"]');
            if (next) next.focus();
        });
        VIEWS.forEach((v) => {
            els.views.appendChild(h('a', {
                class: 'an-views__link',
                href: viewHref(v.id),
                'data-view': v.id,
                text: v.label,
                onclick: (e) => {
                    e.preventDefault();
                    setView(v.id);
                },
            }));
        });
    }

    function setRange(key) {
        if (state.range === key) return;
        state.range = key;
        render();
    }

    function setView(id, extra) {
        if (!VIEWS.some((v) => v.id === id)) id = 'overview';
        state.view = id;
        if (extra && extra.series !== undefined) state.popular.series = extra.series;
        render();
        if (els.root) {
            const top = els.root.getBoundingClientRect().top;
            if (top < 0) els.root.scrollIntoView({ block: 'start' });
        }
    }

    function drawChrome() {
        const view = VIEWS.find((v) => v.id === state.view);
        els.range.hidden = !!view.noRange;
        els.range.querySelectorAll('.an-range__btn').forEach((btn) => {
            const on = btn.dataset.range === state.range;
            btn.setAttribute('aria-checked', on ? 'true' : 'false');
            btn.tabIndex = on ? 0 : -1;
            btn.classList.toggle('is-active', on);
        });
        els.views.querySelectorAll('.an-views__link').forEach((a) => {
            const on = a.dataset.view === state.view;
            a.classList.toggle('is-active', on);
            if (on) a.setAttribute('aria-current', 'page');
            else a.removeAttribute('aria-current');
        });
        writeHash();
    }

    function drawBanner() {
        if (!els.banner) return;
        clear(els.banner);
        if (state.enabled !== false) {
            els.banner.hidden = true;
            return;
        }
        els.banner.hidden = false;
        const link = h('a', {
            href: viewHref('settings'),
            text: 'Turn it on in Analytics settings',
            onclick: (e) => {
                e.preventDefault();
                setView('settings');
            },
        });
        els.banner.appendChild(h('strong', { text: 'Analytics is off. ' }));
        els.banner.appendChild(document.createTextNode('Nothing new is being recorded; what was recorded before stays viewable here. '));
        if (state.view !== 'settings') els.banner.appendChild(link);
    }

    function drawSince() {
        if (!els.since) return;
        if (state.trackingSince) {
            els.since.textContent = 'Tracking since ' + C.fmtDate(state.trackingSince) + ' · counts body bytes of every request, WebDAV included';
        } else if (state.trackingSince === null && state.enabled !== null) {
            els.since.textContent = 'Nothing recorded yet';
        }
    }

    function writeHash() {
        const p = new URLSearchParams();
        p.set('view', state.view);
        if (state.range !== '30d') p.set('range', state.range);
        if (state.view === 'popular' && state.popular.series) p.set('series', state.popular.series);
        if (state.view === 'traffic') {
            if (state.traffic.by !== 'category') p.set('by', state.traffic.by);
            if (state.traffic.metric !== 'bytes_out') p.set('metric', state.traffic.metric);
            if (state.traffic.category) p.set('category', state.traffic.category);
        }
        try {
            // The page's own path: a bare '#...' resolves against the
            // injected <base href> (the mount root), not this page.
            history.replaceState(null, '', window.location.pathname + window.location.search + '#analytics?' + p.toString());
        } catch (_) { /* a sandboxed frame may refuse */ }
    }

    // A link to a view that also works opened in a new tab: relative to
    // the <base href> (the mount root), like every other link on the page.
    function viewHref(id) {
        return '_admin#analytics?view=' + id;
    }

    function readHash() {
        const hash = window.location.hash || '';
        if (!hash.startsWith('#analytics')) return;
        const i = hash.indexOf('?');
        const p = new URLSearchParams(i >= 0 ? hash.slice(i + 1) : '');
        const view = p.get('view');
        if (view && VIEWS.some((v) => v.id === view)) state.view = view;
        const range = p.get('range');
        if (range && RANGES.some((r) => r.key === range)) state.range = range;
        if (p.get('series')) state.popular.series = p.get('series');
        if (['category', 'protocol', 'method', 'detail'].includes(p.get('by'))) state.traffic.by = p.get('by');
        if (['bytes_out', 'bytes_in', 'requests'].includes(p.get('metric'))) state.traffic.metric = p.get('metric');
        if (p.get('category')) state.traffic.category = p.get('category');
    }

    // ------------------------------------------------------------------
    // Rendering frame
    // ------------------------------------------------------------------

    // Re-render the current view. A refetch of the view on screen holds the
    // previous render, dimmed, until the answer is in: no flash, no jump.
    async function render() {
        if (!state.ready) return;
        const seq = ++state.seq;
        const same = state.renderedView === state.view;
        drawChrome();
        drawBanner();
        // A view's own controls are built when the view opens and kept
        // across its refetches, so a select keeps focus while it reloads.
        if (!same) {
            clear(els.controls);
            const controls = CONTROLS[state.view] ? CONTROLS[state.view]() : null;
            if (controls) els.controls.appendChild(controls);
            els.controls.hidden = !controls;
        }
        if (same && els.body.firstChild) {
            els.body.classList.add('is-refreshing');
            els.body.setAttribute('aria-busy', 'true');
        } else {
            clear(els.body);
            els.body.appendChild(h('div', { class: 'an-loading', role: 'status', text: 'Loading…' }));
        }
        state.renderedView = state.view;
        let node;
        try {
            node = await RENDER[state.view]();
        } catch (err) {
            node = errorState(err, () => render());
        }
        if (seq !== state.seq) return;
        els.body.classList.remove('is-refreshing');
        els.body.removeAttribute('aria-busy');
        clear(els.body);
        els.body.appendChild(node);
    }

    // ------------------------------------------------------------------
    // Shared panels
    // ------------------------------------------------------------------

    function card(title, opts) {
        const o = opts || {};
        const head = h('header', { class: 'an-card__head' }, [
            h('div', { class: 'an-card__titles' }, [
                h('h3', { class: 'an-card__title', text: title }),
                o.sub ? h('p', { class: 'an-card__sub', text: o.sub }) : null,
            ]),
            o.actions ? h('div', { class: 'an-card__actions' }, o.actions) : null,
        ]);
        const body = h('div', { class: 'an-card__body' }, o.body || null);
        const node = h('section', { class: 'card an-card' + (o.wide ? ' an-card--wide' : '') + (o.className ? ' ' + o.className : '') }, [head, body]);
        node.body = body;
        return node;
    }

    function grid(children, cls) {
        return h('div', { class: 'an-grid' + (cls ? ' ' + cls : '') }, children);
    }

    function stack(children) {
        return h('div', { class: 'an-stack' }, children);
    }

    function errorState(err, retry) {
        const status = err && err.status;
        let text;
        if (status === 404) text = 'This server does not answer this analytics view (404). It may be an older version.';
        else if (status === 403) text = 'Admin access required.';
        else text = 'Could not load analytics: ' + ((err && err.message) || 'request failed') + (status ? ' (' + status + ')' : '') + '.';
        return h('div', { class: 'an-state an-state--error', role: 'alert' }, [
            h('p', { text }),
            retry ? h('button', { type: 'button', class: 'btn btn--secondary btn--sm', text: 'Try again', onclick: retry }) : null,
        ]);
    }

    function notCollected(what) {
        return h('div', { class: 'an-state an-state--pending' }, [
            h('p', { class: 'an-state__title', text: 'Not collected yet' }),
            h('p', { text: 'This server does not record ' + what + ' yet. The view fills in once it does.' }),
        ]);
    }

    // A panel body for one endpoint's answer: error, not collected, or the
    // drawn content.
    function panelFor(result, what, draw, retry) {
        if (result.error) return errorState(result.error, retry);
        if (result.data && result.data.available === false) return notCollected(what);
        return draw(result.data);
    }

    function granularityNote(data) {
        const g = data && data.range && data.range.granularity;
        if (g === 'hour') return 'Hourly, in your local time';
        if (g === 'day') return 'Daily, by UTC day';
        return '';
    }

    function gran(data) {
        return (data && data.range && data.range.granularity) || 'day';
    }

    // Series read from a daily table (reading_daily, principal_daily,
    // clients_daily, library_daily, uploads_daily) have one point per UTC
    // day whatever the range's granularity: they are labelled as UTC dates.
    const DAILY = 'day';
    const DAILY_NOTE = 'Daily, by UTC day';

    function rangeLong() {
        const r = RANGES.find((x) => x.key === state.range);
        return r ? r.long : state.range;
    }

    function previousPeriod() {
        const r = RANGES.find((x) => x.key === state.range);
        if (!r || r.key === 'all') return null;
        return 'the ' + r.long.replace('the last ', 'previous ');
    }

    // Bytes shown are body bytes plus what nginx sent for us (estimated
    // from the file size, when offload is on).
    function sent(row) {
        return C.num(row && row.bytes_out) + C.num(row && row.est_bytes_out);
    }

    function estNote(est) {
        return C.num(est) > 0 ? 'Includes ' + C.fmtBytes(est) + ' estimated for files nginx sent (X-Accel-Redirect).' : '';
    }

    function seriesFrom(data, colorFor, dict) {
        return ((data && data.series) || []).map((sr) => ({
            key: sr.key,
            label: named(dict || {}, sr.key, sr.label),
            values: sr.values || [],
            color: colorFor ? colorFor(sr.key) : undefined,
        }));
    }

    // A time series named in the design is either the answer's own
    // `{t, series}` or an object of that shape under its name; a single
    // series may come as `{t, values}`.
    function tsOf(data, key) {
        const sub = data && data[key];
        if (sub && typeof sub === 'object' && !Array.isArray(sub)) {
            if (Array.isArray(sub.series)) return { t: sub.t || data.t || [], series: sub.series };
            if (Array.isArray(sub.values)) return { t: sub.t || data.t || [], series: [{ key, label: sub.label, values: sub.values }] };
        }
        const series = ((data && data.series) || []).filter((sr) => !key || sr.key === key || !data.series.some((x) => x.key === key));
        return { t: (data && data.t) || [], series };
    }

    // The unit of a series the server names by what it measures. Exact keys
    // only: a series keyed by a name (an uploader, a series title such as
    // "cpuguy" or "tiny_size") is always a count.
    const UNIT_KEYS = {
        bytes: 'bytes', size: 'bytes', rss: 'bytes', rss_max: 'bytes', bytes_out: 'bytes', bytes_in: 'bytes',
        minutes: 'minutes', cpu_pct: 'pct', cpu_max: 'pct',
    };
    function kindOf(key) {
        const k = String(key || '');
        return Object.prototype.hasOwnProperty.call(UNIT_KEYS, k) ? UNIT_KEYS[k] : 'count';
    }

    // A set of series that may mix units: one chart per unit, never two
    // y-scales on one plot.
    function chartsByUnit(ts, data, opts) {
        const groups = {};
        (ts.series || []).forEach((sr) => {
            // Small multiples: one chart per unit, or per series when the
            // series differ in scale (14 series beside 285 volumes).
            const kind = kindOf(sr.key);
            const group = opts.perSeries ? kind + ':' + sr.key : kind;
            (groups[group] = groups[group] || []).push(sr);
        });
        const kinds = Object.keys(groups);
        if (!kinds.length) return C.emptyState();
        const figs = kinds.map((group) => {
            const kind = group.split(':')[0];
            const fig = C.timeSeries({
                t: ts.t,
                series: groups[group].map((sr) => ({ key: sr.key, label: sr.label || sr.key, values: sr.values, color: opts.perSeries ? 'var(--an-s1)' : undefined })),
                granularity: opts.granularity || gran(data),
                type: opts.type || 'line',
                kind,
                colorFor: opts.colorFor || colors.generic,
                height: opts.height || 180,
                label: opts.label,
            });
            if (kinds.length === 1) return fig;
            return h('div', { class: 'an-sub' }, [h('h4', { class: 'an-sub__title', text: groups[group].map((sr) => sr.label || sr.key).join(', ') }), fig]);
        });
        return opts.perSeries ? grid(figs, 'an-grid--3') : stack(figs);
    }

    function mono(text) {
        return h('span', { class: 'mono an-mono', text: text || '-' });
    }

    function badge(text, tone) {
        return h('span', { class: 'badge badge--' + (tone || 'muted'), text });
    }

    function principalCell(row) {
        const kind = row.kind || (String(row.principal || '').startsWith('a:') ? 'anon' : String(row.principal || '').startsWith('p:') ? 'processor' : 'user');
        const name = row.label || row.principal || '-';
        // The server's label already says "Anonymous 0b53…": no second badge.
        const kindName = KIND[kind] || kind;
        return h('span', { class: 'an-principal' }, [
            h('span', { class: 'an-principal__name', text: name, title: row.principal || '' }),
            kind !== 'user' && !name.startsWith(kindName) ? badge(kindName, kind === 'processor' ? 'info' : 'muted') : null,
        ]);
    }

    function select(id, label, options, value, onChange) {
        const sel = h('select', { id, class: 'form-select an-select' });
        options.forEach((opt) => sel.appendChild(h('option', { value: opt[0], text: opt[1], selected: opt[0] === value })));
        sel.addEventListener('change', () => onChange(sel.value));
        return h('label', { class: 'an-control', for: id }, [h('span', { class: 'an-control__label', text: label }), sel]);
    }

    function chips(label, options, value, onChange) {
        const group = h('div', { class: 'an-chips', role: 'radiogroup', 'aria-label': label });
        group.appendChild(h('span', { class: 'an-control__label', text: label }));
        options.forEach((opt) => {
            group.appendChild(h('button', {
                type: 'button',
                role: 'radio',
                class: 'an-chip' + (opt[0] === value ? ' is-active' : ''),
                'aria-checked': opt[0] === value ? 'true' : 'false',
                text: opt[1],
                onclick: (e) => {
                    group.querySelectorAll('.an-chip').forEach((b) => {
                        b.classList.toggle('is-active', b === e.currentTarget);
                        b.setAttribute('aria-checked', b === e.currentTarget ? 'true' : 'false');
                    });
                    onChange(opt[0]);
                },
            }));
        });
        return group;
    }

    // ------------------------------------------------------------------
    // Per-view controls (built once per visit, kept across refetches)
    // ------------------------------------------------------------------

    const CONTROLS = {
        traffic() {
            const t = state.traffic;
            const row = h('div', { class: 'an-controls' });
            row.appendChild(select('an-traffic-by', 'Break down by', [
                ['category', 'Category'], ['protocol', 'Protocol (WebDAV, web, API)'], ['method', 'Method'], ['detail', 'Detail'],
            ], t.by, (v) => { t.by = v; render(); }));
            row.appendChild(select('an-traffic-metric', 'Measure', [
                ['bytes_out', 'Bytes sent'], ['bytes_in', 'Bytes received'], ['requests', 'Requests'],
            ], t.metric, (v) => { t.metric = v; render(); }));
            const catOptions = [['', 'All categories']].concat(Object.keys(CATEGORY).map((k) => [k, CATEGORY[k]]));
            row.appendChild(select('an-traffic-category', 'Only', catOptions, t.category, (v) => { t.category = v; render(); }));
            return row;
        },
        popular() {
            const p = state.popular;
            const row = h('div', { class: 'an-controls' });
            const input = h('input', { type: 'search', id: 'an-popular-q', class: 'form-input an-search', placeholder: 'Filter series', value: p.q, 'aria-label': 'Filter series by title' });
            let timer = null;
            input.addEventListener('input', () => {
                clearTimeout(timer);
                timer = setTimeout(() => { p.q = input.value.trim(); render(); }, 300);
            });
            row.appendChild(h('label', { class: 'an-control an-control--grow', for: 'an-popular-q' }, [h('span', { class: 'an-control__label', text: 'Series' }), input]));
            row.appendChild(select('an-popular-sort', 'Sort by', [
                ['downloads', 'Downloads'], ['downloaders', 'Downloaders'], ['readers', 'Readers'], ['reads', 'Reads'],
                ['minutes', 'Reading time'], ['pages', 'Pages read'], ['bytes', 'Bytes sent'],
            ], p.sort, (v) => { p.sort = v; render(); }));
            return row;
        },
        health() {
            return h('div', { class: 'an-controls' }, chips('Protocol', [
                ['', 'All'], ['dav', 'WebDAV'], ['web', 'Web pages'], ['api', 'JSON API'], ['processor', 'Processors'],
            ], state.health.protocol, (v) => { state.health.protocol = v; render(); }));
        },
        clients() {
            return h('div', { class: 'an-controls' }, select('an-clients-metric', 'Measure', [
                ['requests', 'Requests'], ['bytes_out', 'Bytes sent'], ['principals', 'Distinct visitors'],
            ], state.clients.metric, (v) => { state.clients.metric = v; render(); }));
        },
    };

    // ------------------------------------------------------------------
    // Overview
    // ------------------------------------------------------------------

    async function renderOverview() {
        const data = await get('overview');
        if (data.available === false) return notCollected('traffic');
        const k = data.kpis || {};
        const bw = data.bandwidth || {};
        const totals = (bw.t || []).map((_, i) => (bw.series || []).reduce((a, sr) => a + C.num((sr.values || [])[i]), 0));
        const tiles = C.kpis([
            { label: 'Sent', value: C.fmtBytes(sent(k)), sub: C.num(k.est_bytes_out) > 0 ? 'incl. ' + C.fmtBytes(k.est_bytes_out) + ' estimated' : 'body bytes, all protocols', spark: totals },
            { label: 'Received', value: C.fmtBytes(k.bytes_in), sub: 'uploads and sync' },
            { label: 'Requests', value: C.fmtNum(k.requests), sub: 'WebDAV, web and API' },
            { label: 'Downloads', value: C.fmtNum(k.downloads), sub: many(k.downloaders, 'downloader', C.fmtNum) },
            { label: 'Reads', value: C.fmtNum(k.reads), sub: many(k.readers, 'reader', C.fmtNum) },
            { label: 'Reading time', value: C.fmtMinutes(k.minutes_read), sub: many(k.pages_read, 'page', C.fmtNum) },
            { label: 'Active today', value: C.fmtNum(k.dau), sub: C.fmtNum(k.wau) + ' this week · ' + C.fmtNum(k.mau) + ' this month (accounts and anonymous visitors)' },
            { label: 'Crawl episodes', value: C.fmtNum(k.crawl_episodes), tone: C.num(k.crawl_episodes) > 0 ? 'warn' : null, sub: C.num(k.crawl_episodes) > 0 ? 'see Crawlers' : 'none detected' },
            { label: 'Server errors (5xx)', value: C.fmtNum(k.errors_5xx), tone: C.num(k.errors_5xx) > 0 ? 'bad' : null },
            { label: 'Failed sign-ins', value: C.fmtNum(k.auth_failures), tone: C.num(k.auth_failures) > 20 ? 'warn' : null },
            { label: 'Memory', value: k.rss_bytes ? C.fmtBytes(k.rss_bytes) : '-', sub: k.rss_peak_bytes ? 'peak ' + C.fmtBytes(k.rss_peak_bytes) : 'not reported' },
            { label: 'CPU', value: k.cpu_pct === undefined || k.cpu_pct === null ? '-' : C.fmtPct(k.cpu_pct), sub: k.uptime_s ? 'up ' + C.fmtDuration(k.uptime_s) : null },
        ]);
        const notes = [];
        if (C.num(k.dropped) > 0) notes.push(h('p', { class: 'an-note an-note--warn', text: C.fmtNum(k.dropped) + ' requests were folded into overflow counters (the per-minute key caps were reached); totals still include them.' }));

        const bandwidth = card('Bandwidth by category', {
            sub: [granularityNote(data), estNote(k.est_bytes_out)].filter(Boolean).join(' · '),
            actions: [viewLink('traffic', 'Traffic')],
            body: C.timeSeries({
                t: bw.t, series: seriesFrom(bw, colors.category, CATEGORY), granularity: gran(data),
                type: 'area', kind: 'bytes', colorFor: colors.category, label: 'Bytes sent by category', totalLabel: 'All categories',
            }),
            wide: true,
        });
        const topSeries = card('Most downloaded series', {
            sub: rangeLong(),
            actions: [viewLink('popular', 'Popular')],
            body: C.barList((data.top_series || []).map((r) => ({
                key: r.series_key, label: r.title || r.series_key, value: r.downloads,
                sub: many(r.readers, 'reader', C.fmtNum) + ' · ' + C.fmtMinutes(r.minutes) + ' read',
            })), { kind: 'count', emptyText: 'No downloads in this range.', label: 'Downloads per series', onSelect: (row) => setView('popular', { series: row.key }) }),
        });
        const topPeople = card('Most bandwidth', {
            sub: 'Accounts and anonymous visitors, ' + rangeLong(),
            actions: [viewLink('users', 'Users')],
            body: C.barList((data.top_principals || []).map((r) => ({
                key: r.principal, label: r.label || r.principal, value: r.bytes_out, title: r.principal,
                badge: r.kind && r.kind !== 'user' && !String(r.label || '').startsWith(KIND[r.kind] || r.kind) ? KIND[r.kind] || r.kind : null,
            })), { kind: 'bytes', emptyText: 'No traffic in this range.', label: 'Bytes sent per visitor' }),
        });
        return stack([tiles].concat(notes, [bandwidth, grid([topSeries, topPeople])]));
    }

    function viewLink(view, label) {
        return h('a', {
            class: 'an-link',
            href: viewHref(view),
            text: label + ' →',
            onclick: (e) => { e.preventDefault(); setView(view); },
        });
    }

    // ------------------------------------------------------------------
    // Traffic
    // ------------------------------------------------------------------

    const METRIC = { bytes_out: 'Bytes sent', bytes_in: 'Bytes received', requests: 'Requests' };
    const BY = { category: 'category', protocol: 'protocol', method: 'method', detail: 'detail' };

    async function renderTraffic() {
        const t = state.traffic;
        const data = await get('traffic', { by: t.by, metric: t.metric, category: t.category });
        if (data.available === false) return notCollected('traffic');
        const kind = t.metric === 'requests' ? 'count' : 'bytes';
        const colorFor = byColors(t.by);
        const dict = byDict(t.by);
        const totalEst = (data.totals || []).reduce((a, r) => a + C.num(r.est_bytes_out), 0);
        const scope = t.category ? ' · only ' + named(CATEGORY, t.category) : '';
        const chart = card(METRIC[t.metric] + ' by ' + BY[t.by] + scope, {
            sub: [granularityNote(data), t.metric === 'bytes_out' ? estNote(totalEst) : ''].filter(Boolean).join(' · '),
            body: C.timeSeries({
                t: data.t, series: seriesFrom(data, colorFor, dict), granularity: gran(data), type: 'area', kind,
                colorFor, label: METRIC[t.metric] + ' by ' + BY[t.by], totalLabel: 'All',
            }),
            wide: true,
        });

        const totalsTable = C.table([
            { key: 'label', label: named({ category: 'Category', protocol: 'Protocol', method: 'Method', detail: 'Detail' }, t.by), render: (r) => keyed(r.label || named(dict, r.key), colorFor(r.key)) },
            { key: 'requests', label: 'Requests', kind: 'int' },
            { key: 'sent', label: 'Sent', kind: 'bytes', sortValue: sent, render: (r) => C.fmtBytes(sent(r)) },
            { key: 'bytes_in', label: 'Received', kind: 'bytes' },
        ], {
            rows: data.totals || [],
            sortKey: t.metric === 'requests' ? 'requests' : t.metric === 'bytes_in' ? 'bytes_in' : 'sent',
            caption: 'Totals by ' + BY[t.by],
            onRowClick: t.by === 'category' ? (r) => {
                t.by = 'detail';
                t.category = r.key;
                syncTrafficControls();
                render();
            } : null,
            rowLabel: (r) => 'Break ' + (r.label || r.key) + ' down by detail',
        });
        const totals = card('Totals', {
            sub: t.by === 'category' ? 'Select a category to break it down by detail.' : rangeLong(),
            body: totalsTable,
        });

        return stack([chart, grid([totals, davCard(data, kind)]), grid([syncCard('Progress sync', 'volume-data.json, positions.json and reading history', data.sync && data.sync.progress),
            syncCard('Profile sync', 'profiles.json and goals.json', data.sync && data.sync.profile), metadataCard(data.metadata)], 'an-grid--3')]);
    }

    function syncTrafficControls() {
        const by = document.getElementById('an-traffic-by');
        const cat = document.getElementById('an-traffic-category');
        if (by) by.value = state.traffic.by;
        if (cat) cat.value = state.traffic.category;
    }

    function keyed(label, color) {
        const sw = h('span', { class: 'an-key', 'aria-hidden': 'true' });
        sw.style.setProperty('--c', color || C.OTHER);
        return h('span', { class: 'an-keyed' }, [sw, h('span', { text: label })]);
    }

    // WebDAV, method by method: the reader's PROPFIND listings, GETs of
    // archives and sidecars, PUTs of sync files, and generic clients.
    function davCard(data, kind) {
        const rows = data.dav_methods || [];
        const sum = rows.reduce((a, r) => ({ requests: a.requests + C.num(r.requests), out: a.out + C.num(r.bytes_out), inn: a.inn + C.num(r.bytes_in) }), { requests: 0, out: 0, inn: 0 });
        const metric = state.traffic.metric;
        const body = rows.length ? [
            C.kpis([
                { label: 'WebDAV requests', value: C.fmtNum(sum.requests) },
                { label: 'Sent over WebDAV', value: C.fmtBytes(sum.out) },
                { label: 'Received over WebDAV', value: C.fmtBytes(sum.inn) },
            ]),
            C.barList(rows.map((r) => ({
                key: r.method, label: r.method, value: metric === 'requests' ? r.requests : metric === 'bytes_in' ? r.bytes_in : r.bytes_out,
                color: colors.method(r.method) || C.OTHER,
                sub: many(r.requests, 'request') + ' · ' + C.fmtBytes(r.bytes_out) + ' sent · ' + C.fmtBytes(r.bytes_in) + ' received',
            })).sort((a, b) => b.value - a.value), { kind, label: 'WebDAV by method' }),
            C.tableToggle(() => C.dataTable(['Method', 'Requests', 'Sent', 'Received'], rows.map((r) => [r.method, C.fmtInt(r.requests), C.fmtBytes(r.bytes_out), C.fmtBytes(r.bytes_in)]), 'WebDAV by method')),
        ] : C.emptyState('No WebDAV requests in this range.');
        return card('WebDAV by method', { sub: 'Every WebDAV method, ' + rangeLong(), body });
    }

    function syncCard(title, sub, part) {
        if (!part) return card(title, { sub, body: C.emptyState() });
        return card(title, {
            sub,
            body: [
                C.kpis([
                    { label: 'Sent', value: C.fmtBytes(part.bytes_out) },
                    { label: 'Received', value: C.fmtBytes(part.bytes_in) },
                    { label: 'Requests', value: C.fmtNum(part.requests) },
                ]),
                C.barList((part.by_detail || []).map((d) => ({
                    key: d.key, label: named(DETAIL, d.key, d.label), value: C.num(d.bytes_out) + C.num(d.bytes_in),
                    sub: many(d.requests, 'request') + ' · ' + C.fmtBytes(d.bytes_out) + ' sent · ' + C.fmtBytes(d.bytes_in) + ' received',
                })), { kind: 'bytes', label: title + ' by file', emptyText: 'No sync traffic in this range.' }),
            ],
        });
    }

    function metadataCard(meta) {
        if (!meta) return card('Navigation & metadata', { body: C.emptyState() });
        return card('Navigation & metadata', {
            sub: 'PROPFIND listings, series.json, catalog and manifests',
            body: [
                C.kpis([
                    { label: 'Sent', value: C.fmtBytes(meta.bytes_out) },
                    { label: 'Requests', value: C.fmtNum(meta.requests) },
                ]),
                C.barList((meta.by_detail || []).map((d) => ({
                    key: d.key, label: named(DETAIL, d.key, d.label), value: d.bytes_out,
                    sub: many(d.requests, 'request'),
                })), { kind: 'bytes', label: 'Metadata by kind', emptyText: 'No navigation traffic in this range.' }),
            ],
        });
    }

    // ------------------------------------------------------------------
    // Popular
    // ------------------------------------------------------------------

    async function renderPopular() {
        const p = state.popular;
        const data = await get('series', { sort: p.sort, limit: 50, q: p.q });
        if (data.available === false) return notCollected('downloads or reading');
        const period = previousPeriod();
        const rows = data.rows || [];
        const funnelSlot = h('div', { class: 'an-funnel-slot' });
        const columns = [
            { key: 'title', label: 'Series', sortable: false, className: 'an-col-title', render: (r) => h('span', { class: 'an-title', text: r.title || r.series_key, title: r.series_key }) },
            { key: 'downloads', label: 'Downloads', num: true, render: (r) => withTrend(r.downloads, r.prev && r.prev.downloads, period) },
            { key: 'downloaders', label: 'Downloaders', kind: 'int' },
            { key: 'readers', label: 'Readers', num: true, render: (r) => withTrend(r.readers, r.prev && r.prev.readers, period) },
            { key: 'reads', label: 'Reads', kind: 'int' },
            { key: 'minutes', label: 'Reading time', kind: 'minutes' },
            { key: 'pages', label: 'Pages', kind: 'int' },
            { key: 'bytes', label: 'Sent', num: true, render: (r) => C.fmtBytes(r.bytes_out) },
            { key: 'volumes', label: 'Volumes', kind: 'int', sortable: false },
        ];
        let tableNode;
        const selectRow = (r) => {
            p.series = r.series_key;
            writeHash();
            tableNode.querySelectorAll('tr.is-selected').forEach((tr) => tr.classList.remove('is-selected'));
            const tr = Array.from(tableNode.querySelectorAll('tbody tr')).find((x) => x.dataset.key === r.series_key);
            if (tr) tr.classList.add('is-selected');
            loadFunnel(funnelSlot, r.series_key, r.title, true);
        };
        tableNode = C.table(columns, {
            rows,
            sortKey: p.sort,
            serverSort: true,
            onSort: (key) => {
                p.sort = key;
                const sel = document.getElementById('an-popular-sort');
                if (sel) sel.value = key;
                render();
            },
            onRowClick: selectRow,
            rowKey: (r) => r.series_key,
            selectedKey: p.series,
            rowLabel: (r) => 'Show the completion funnel of ' + (r.title || r.series_key),
            emptyText: p.q ? 'No series match “' + p.q + '”.' : 'No downloads or reads in this range.',
            caption: 'Series by ' + p.sort,
        });
        tableNode.querySelectorAll('tbody tr').forEach((tr, i) => { if (rows[i]) tr.dataset.key = rows[i].series_key; });
        const list = card('Series', {
            sub: rows.length ? 'Top ' + rows.length + ', ' + rangeLong() + (period ? '; arrows compare with ' + period : '') + '. Select a series for its completion funnel.' : rangeLong(),
            body: tableNode,
            wide: true,
        });
        if (p.series) loadFunnel(funnelSlot, p.series, (rows.find((r) => r.series_key === p.series) || {}).title, false);
        return stack([list, funnelSlot]);
    }

    function withTrend(cur, prev, period) {
        const wrap = h('span', { class: 'an-trendcell' }, [h('span', { text: C.fmtInt(cur) })]);
        if (period && prev !== undefined && prev !== null) wrap.appendChild(C.trend(cur, prev, true, period));
        return wrap;
    }

    let funnelSeq = 0;
    async function loadFunnel(slot, seriesKey, title, scroll) {
        const seq = ++funnelSeq;
        clear(slot);
        const panel = card('Completion funnel: ' + (title || seriesKey), {
            sub: 'Per volume, in library order: people who downloaded it, started it and finished it (all time).',
            actions: [h('button', { type: 'button', class: 'btn btn--ghost btn--sm', text: 'Close', onclick: () => { state.popular.series = ''; writeHash(); clear(slot); } })],
            body: h('div', { class: 'an-loading', role: 'status', text: 'Loading…' }),
            wide: true,
        });
        slot.appendChild(panel);
        if (scroll) panel.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
        const result = await tryGet('funnel', { series: seriesKey });
        if (seq !== funnelSeq) return;
        clear(panel.body);
        // A series with nothing recorded (renamed, removed, or a stale link):
        // said as such, and forgotten so a revisit does not ask again. A 404
        // is how an older server answers it.
        const forget = () => {
            if (state.popular.series === seriesKey) {
                state.popular.series = '';
                if (state.view === 'popular') writeHash();
            }
        };
        const unknown = h('div', { class: 'an-state an-state--pending' }, [
            h('p', { class: 'an-state__title', text: 'Nothing recorded for this series' }),
            h('p', { text: 'No volume of it is in the library, and nobody downloaded or read one. It may have been renamed or removed.' }),
        ]);
        if (result.error && result.error.status === 404) {
            forget();
            panel.body.appendChild(unknown);
            return;
        }
        panel.body.appendChild(panelFor(result, 'reading progress', (data) => {
            const head = panel.querySelector('.an-card__title');
            if (data.title) head.textContent = 'Completion funnel: ' + data.title;
            const vols = data.volumes || [];
            if (!vols.length && data.in_library === false) {
                forget();
                return unknown;
            }
            const last = vols.length ? vols[vols.length - 1] : null;
            return stack([
                C.kpis([
                    { label: 'Started reading', value: C.fmtNum(data.readers_started), sub: 'people' },
                    { label: 'Finished a volume', value: C.fmtNum(data.readers_finished), sub: 'people' },
                    { label: 'Volumes', value: C.fmtNum(vols.length) },
                    last ? { label: 'Finished the latest', value: C.fmtNum(last.finished), sub: last.title || last.volume_key } : null,
                ]),
                C.funnel(vols),
                stagesCard(data.stages),
            ]);
        }, () => loadFunnel(slot, seriesKey, title, false)));
    }

    // How far each (person, volume) read got: started, a quarter, half, three
    // quarters, finished. Each stage holds the ones after it.
    function stagesCard(stages) {
        if (!Array.isArray(stages) || !stages.length) return null;
        const seq = ['var(--an-seq-1)', 'var(--an-seq-2)', 'var(--an-seq-2)', 'var(--an-seq-3)', 'var(--an-seq-3)'];
        return h('div', { class: 'an-sub' }, [
            h('h4', { class: 'an-sub__title', text: 'How far reads got' }),
            C.barList(stages.map((st, i) => ({ key: st.key, label: st.label || st.key, value: st.reads, color: seq[i] || 'var(--an-s1)' })), {
                kind: 'count', label: 'Reads by how far they got', emptyText: 'Nobody started a volume of this series yet.',
            }),
            h('p', { class: 'an-note', text: 'One read is one person and one volume. A volume whose page count is not known counts as started or finished only.' }),
        ]);
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    async function renderReading() {
        const data = await get('reading');
        if (data.available === false) return notCollected('reading activity');
        const find = (key) => (data.series || []).find((sr) => sr.key === key) || { key, values: [] };
        const sum = (sr) => (sr.values || []).reduce((a, v) => a + C.num(v), 0);
        const minutes = find('minutes');
        const pages = find('pages');
        const readers = find('readers');
        const g = DAILY;
        const small = (sr, label, kind) => card(label, {
            sub: DAILY_NOTE,
            body: C.timeSeries({ t: data.t, series: [{ key: sr.key, label, values: sr.values, color: 'var(--an-s1)' }], granularity: g, type: g === 'day' && (data.t || []).length <= 120 ? 'bar' : 'line', kind, height: 160, label }),
        });
        const sources = data.sources || {};
        const totals = data.totals || null;
        return stack([
            C.kpis([
                { label: 'Reading time', value: C.fmtMinutes(totals ? totals.minutes : sum(minutes)), spark: minutes.values },
                { label: 'Pages read', value: C.fmtNum(totals ? totals.pages : sum(pages)), spark: pages.values },
                totals ? { label: 'Reads', value: C.fmtNum(totals.reads), sub: many(totals.readers, 'reader', C.fmtNum) } : null,
                { label: 'Most readers in one day', value: C.fmtNum(Math.max(0, ...(readers.values || []).map(C.num))) },
            ]),
            grid([small(minutes, 'Minutes read', 'minutes'), small(pages, 'Pages read', 'count'), small(readers, 'Readers', 'count')], 'an-grid--3'),
            grid([
                card('Top readers', {
                    sub: rangeLong(),
                    body: C.table([
                        { key: 'username', label: 'Account' },
                        { key: 'minutes', label: 'Time', kind: 'minutes' },
                        { key: 'pages', label: 'Pages', kind: 'int' },
                        { key: 'volumes', label: 'Volumes', kind: 'int' },
                    ], { rows: data.top_readers || [], sortKey: 'minutes', emptyText: 'Nobody read in this range.' }),
                }),
                card('Most read volumes', {
                    sub: rangeLong(),
                    body: C.table([
                        { key: 'title', label: 'Volume', render: (r) => h('span', { class: 'an-title' }, [h('span', { text: r.title || r.volume_key, title: r.volume_key }), r.series_title ? h('span', { class: 'an-muted', text: r.series_title }) : null]) },
                        { key: 'readers', label: 'Readers', kind: 'int' },
                        { key: 'minutes', label: 'Time', kind: 'minutes' },
                    ], { rows: data.top_volumes || [], sortKey: 'readers', emptyText: 'No volume was read in this range.' }),
                }),
            ]),
            card('Where the numbers come from', {
                sub: 'Reads are counted from the reader\'s synced files: positions.json (last page per volume) and the reading-history log (pages and time per session).',
                body: [
                    C.segmented([
                        { key: 'positions', label: 'From positions.json', value: sources.positions, color: 'var(--an-s1)' },
                        { key: 'history', label: 'From reading history', value: sources.history, color: 'var(--an-s2)' },
                    ], { kind: 'int', tall: true, label: 'Reads by source', emptyText: 'No reads recorded yet.' }),
                    workerNote(data.worker),
                ],
            }),
        ]);
    }

    // Synced files the reading worker could not use, since this server started.
    function workerNote(w) {
        if (!w || typeof w !== 'object') return null;
        const parts = [
            [w.too_large, 'larger than the reader writes'],
            [w.unknown_format, 'of a newer format'],
            [w.malformed, 'malformed or on another device\'s path'],
            [w.failed, 'unreadable'],
            [C.num(w.dropped) + C.num(w.pending_dropped), 'dropped while the worker was busy'],
        ].filter((p) => C.num(p[0]) > 0).map((p) => C.fmtNum(p[0]) + ' ' + p[1]);
        const late = C.num(w.events_out_of_range);
        if (!parts.length && !late) {
            return h('p', { class: 'an-note', text: C.fmtNum(w.files) + ' synced files read since the server started; none skipped.' });
        }
        const text = (parts.length ? 'Synced files skipped since the server started: ' + parts.join(', ') + '.' : '') +
            (late ? (parts.length ? ' ' : '') + C.fmtNum(late) + ' history events with an impossible date (a broken device clock) were left out.' : '');
        return h('p', { class: 'an-note an-note--warn', text });
    }

    // ------------------------------------------------------------------
    // Users
    // ------------------------------------------------------------------

    async function renderUsers() {
        const [users, principals] = await Promise.all([tryGet('users'), tryGet('principals', { limit: 50 })]);
        const parts = [];
        parts.push(panelFor(users, 'active users', (data) => {
            const rows = data.rows || [];
            const dau = (data.series || []).find((sr) => sr.key === 'dau') || (data.series || [])[0] || { values: [] };
            const g = DAILY;
            // `accounts` counts every account of the range; the rows stop at
            // `rows_limit` (an older server sends neither).
            const seen = typeof data.accounts === 'number' ? data.accounts : rows.length;
            const capped = seen > rows.length;
            return stack([
                C.kpis([
                    { label: 'Active this week', value: C.fmtNum(data.wau), sub: 'distinct accounts, last 7 days' },
                    { label: 'Active this month', value: C.fmtNum(data.mau), sub: 'distinct accounts, last 30 days' },
                    { label: 'Accounts seen', value: C.fmtNum(seen), sub: rangeLong() },
                ]),
                card('Daily active accounts', {
                    sub: DAILY_NOTE,
                    body: C.timeSeries({ t: data.t, series: [{ key: 'dau', label: 'Active accounts', values: dau.values, color: 'var(--an-s1)' }], granularity: g, type: g === 'day' && (data.t || []).length <= 120 ? 'bar' : 'line', kind: 'count', height: 180, label: 'Active accounts' }),
                    wide: true,
                }),
                card('Accounts', {
                    sub: 'Signed-in use, ' + rangeLong() + '. Devices come from the reader\'s synced files.' +
                        (capped ? ' Listing the ' + C.fmtNum(rows.length) + ' with the most traffic of ' + C.fmtNum(seen) + '.' : ''),
                    body: C.table([
                        { key: 'username', label: 'Account' },
                        { key: 'last_seen', label: 'Last seen', num: true, render: (r) => h('span', { text: C.fmtAgo(r.last_seen), title: C.fmtDateTime(r.last_seen) }) },
                        { key: 'days_active', label: 'Days active', kind: 'int' },
                        { key: 'requests', label: 'Requests', kind: 'int' },
                        { key: 'bytes_out', label: 'Sent', kind: 'bytes' },
                        { key: 'minutes', label: 'Reading', num: true, sortValue: (r) => C.num(r.minutes), render: readingCell },
                        { key: 'volumes_completed', label: 'Finished', num: true, sortValue: (r) => C.num(r.volumes_completed), render: finishedCell },
                        { key: 'devices', label: 'Devices', sortValue: (r) => (r.devices || []).length, render: devicesCell },
                        { key: 'clients', label: 'Clients', sortable: false, render: clientsCell },
                    ], { rows, sortKey: 'last_seen', emptyText: 'No account was active in this range.' }),
                    wide: true,
                }),
            ]);
        }, () => render()));
        parts.push(panelFor(principals, 'per-visitor bandwidth', (data) => {
            const tot = data.totals || {};
            return card('Bandwidth by visitor', {
                sub: 'Accounts, anonymous visitors (a pseudonymous id per IP) and OCR processors, ' + rangeLong() + '. IPs of anonymous visitors are kept for the IP retention period only.',
                body: [
                    C.kpis([
                        { label: 'Accounts', value: C.fmtNum(tot.users) },
                        { label: 'Anonymous visitors', value: C.fmtNum(tot.anon) },
                        { label: 'Processors', value: C.fmtNum(tot.processor) },
                    ]),
                    C.table([
                        { key: 'label', label: 'Visitor', render: principalCell },
                        { key: 'requests', label: 'Requests', kind: 'int' },
                        { key: 'bytes_out', label: 'Sent', kind: 'bytes' },
                        { key: 'bytes_in', label: 'Received', kind: 'bytes' },
                        { key: 'by_category', label: 'Sent by category', sortable: false, className: 'an-col-seg', render: (r) => categoryBar(r.by_category) },
                        { key: 'last_seen', label: 'Last seen', num: true, render: (r) => h('span', { text: C.fmtAgo(r.last_seen), title: C.fmtDateTime(r.last_seen) }) },
                        { key: 'ips', label: 'IPs', sortable: false, render: (r) => (r.ips && r.ips.length ? h('span', { class: 'an-ips' }, r.ips.slice(0, 3).map((ip) => mono(ip))) : '-') },
                    ], { rows: data.rows || [], sortKey: 'bytes_out', emptyText: 'No traffic in this range.' }),
                ],
                wide: true,
            });
        }, () => render()));
        return stack(parts);
    }

    // Time and pages read in the range (older servers send neither).
    function readingCell(row) {
        if (row.minutes === undefined && row.pages === undefined) return '-';
        if (!C.num(row.minutes) && !C.num(row.pages)) return '-';
        return h('span', { class: 'an-title', title: C.fmtInt(row.volumes_read) + ' volumes read' }, [
            h('span', { text: C.fmtMinutes(row.minutes) }),
            h('span', { class: 'an-muted', text: many(row.pages, 'page') }),
        ]);
    }

    // Volumes finished of those started (all time, from volume-data.json).
    function finishedCell(row) {
        if (row.volumes_started === undefined) return '-';
        if (!C.num(row.volumes_started) && !C.num(row.volumes_completed)) return '-';
        return h('span', { text: C.fmtInt(row.volumes_completed) + ' of ' + C.fmtInt(row.volumes_started), title: 'Volumes finished of the volumes started (all time)' });
    }

    function devicesCell(row) {
        const devices = row.devices || [];
        if (!devices.length) return '-';
        const list = h('ul', { class: 'an-devices' }, devices.map((d) => h('li', null, [
            h('span', { text: d.label || d.id, title: d.id }),
            h('span', { class: 'an-muted', text: ' · last ' + C.fmtAgo(d.last_seen) }),
        ])));
        return h('details', { class: 'an-details' }, [h('summary', { text: devices.length + (devices.length === 1 ? ' device' : ' devices') }), list]);
    }

    function clientsCell(row) {
        const clients = (row.clients || []).map((c) => {
            if (typeof c === 'string') return named(APP, c, null);
            return c.label || named(APP, c.app, null) + (c.client && c.client !== 'browser' ? ' (' + named(CLIENT, c.client) + ')' : '');
        });
        if (!clients.length) return '-';
        return h('span', { class: 'an-tags' }, clients.slice(0, 4).map((c) => badge(c)));
    }

    function categoryBar(byCat) {
        if (!byCat || typeof byCat !== 'object') return '-';
        const parts = Object.keys(byCat).map((key) => {
            const v = byCat[key];
            const value = typeof v === 'object' && v ? C.num(v.bytes_out) : C.num(v);
            return { key, label: named(CATEGORY, key), value, color: colors.category(key) || C.OTHER };
        }).sort((a, b) => b.value - a.value);
        if (!parts.some((p) => p.value > 0)) return '-';
        return C.segmented(parts, { kind: 'bytes', legend: false, label: 'Sent by category' });
    }

    // ------------------------------------------------------------------
    // Library
    // ------------------------------------------------------------------

    async function renderLibrary() {
        const data = await get('library');
        if (data.available === false) return notCollected('library scans');
        const totals = data.totals || {};
        const rescan = h('button', { type: 'button', class: 'btn btn--secondary btn--sm', text: 'Rescan now' });
        rescan.addEventListener('click', async () => {
            rescan.disabled = true;
            try {
                await apiPost('/analytics/library/rescan', {});
                showToast('Library rescan started; it may take a while on a large library', 'success');
                setTimeout(() => { if (state.view === 'library') render(); }, 1500);
            } catch (err) {
                showToast('Could not start the rescan: ' + err.message, 'error');
            } finally {
                rescan.disabled = false;
            }
        });
        const scanned = data.scanned_at ? 'Scanned ' + C.fmtAgo(data.scanned_at) + ' (' + C.fmtDateTime(data.scanned_at) + ')' : 'Not scanned yet';
        const scan = data.scan || {};
        const PHASE = { files: 'counting files', pages: 'reading page formats' };
        const running = data.scanning ? 'scanning now' + (scan.phase && PHASE[scan.phase] ? ' (' + PHASE[scan.phase] + ')' : '') : '';
        const every = scan.interval_hours ? 'every ' + C.fmtNum(scan.interval_hours) + ' h' : '';
        const noSidecar = data.no_sidecar || null;
        const headNotes = [];
        if (scan.error) headNotes.push(h('p', { class: 'an-note an-note--bad', role: 'alert', text: 'The last scan failed: ' + scan.error }));
        if (scan.waiting) headNotes.push(h('p', { class: 'an-note', text: 'Waiting: ' + scan.waiting + '.' }));
        if (C.num(data.loose_archives) > 0) headNotes.push(h('p', { class: 'an-note', text: C.fmtNum(data.loose_archives) + (C.num(data.loose_archives) === 1 ? ' archive sits' : ' archives sit') + ' in the library root, outside any series folder; they are not counted as volumes.' }));
        const head = card('Library', {
            sub: [scanned, running, every, data.cloud ? 'cloud library (read from the store\'s index, nothing downloaded)' : ''].filter(Boolean).join(' · '),
            actions: [rescan],
            body: [C.kpis([
                { label: 'Series', value: C.fmtNum(totals.series) },
                { label: 'Volumes', value: C.fmtNum(totals.volumes) },
                { label: 'Size', value: C.fmtBytes(totals.bytes), sub: totals.files !== undefined ? many(totals.files, 'file', C.fmtNum) : null },
                data.dormant ? { label: 'Dormant series', value: C.fmtNum(data.dormant.count), sub: 'no reads or downloads in ' + (data.dormant.days || 90) + ' days' } : null,
                noSidecar ? { label: 'Without OCR', value: C.fmtNum(noSidecar.count), sub: 'series with no sidecar at all' } : null,
            ])].concat(headNotes),
            wide: true,
        });
        const kinds = data.by_kind || [];
        const byKind = card('Files by kind', {
            body: [
                C.segmented(kinds.map((k) => ({ key: k.kind, label: named(LIB_KIND, k.kind, k.label), value: k.bytes, color: colors.libkind(k.kind) || C.OTHER })), { kind: 'bytes', tall: true, label: 'Bytes by kind' }),
                C.table([
                    { key: 'label', label: 'Kind', render: (r) => r.label || named(LIB_KIND, r.kind) },
                    { key: 'files', label: 'Files', kind: 'int' },
                    { key: 'bytes', label: 'Size', kind: 'bytes' },
                ], { rows: kinds, sortKey: 'bytes', emptyText: 'Nothing scanned yet.' }),
            ],
        });
        const byExt = card('Files by extension', {
            body: C.table([
                { key: 'ext', label: 'Extension', render: (r) => mono(r.ext ? '.' + String(r.ext).replace(/^\./, '') : '(none)') },
                { key: 'files', label: 'Files', kind: 'int' },
                { key: 'bytes', label: 'Size', kind: 'bytes' },
            ], { rows: data.by_ext || [], sortKey: 'bytes', emptyText: 'Nothing scanned yet.' }),
        });
        const mix = card('Archive types per series', {
            sub: 'Which archive formats each series folder holds',
            body: C.barList((data.archive_mix || []).map((m) => ({ key: m.mix, label: m.mix, value: m.series })), { kind: 'count', valueKind: 'count', emptyText: 'Nothing scanned yet.' }),
        });
        return stack([head, grid([byKind, byExt]), grid([mix, pageFormatsCard(data.page_formats)]), growthCard(data), uploadsCard(data),
            grid([dormantCard(data.dormant), noSidecarCard(noSidecar)])]);
    }

    function noSidecarCard(ns) {
        if (!ns) return null;
        return card('Series without OCR', {
            sub: 'Series folders holding no .mokuro sidecar at all',
            body: C.table([
                { key: 'title', label: 'Series', render: (r) => h('span', { class: 'an-title', text: r.title || r.series_key, title: r.series_key }) },
                { key: 'volumes', label: 'Volumes', kind: 'int' },
            ], { rows: ns.series || [], sortKey: 'volumes', emptyText: 'Every series has OCR.', limit: 200 }),
        });
    }

    function pageFormatsCard(pf) {
        if (!pf) return card('Page image formats', { body: C.emptyState('Not reported.') });
        if (pf.available === false) {
            const why = pf.reason === 'cloud'
                ? 'Page formats are read from each archive\'s zip directory, which needs a local library; this library is in cloud storage, and reading them would mean downloading every archive.'
                : pf.reason === 'disabled' ? 'Turned off in Analytics settings (Read page formats).' : 'Not available' + (pf.reason ? ': ' + pf.reason : '') + '.';
            return card('Page image formats', { body: h('div', { class: 'an-state an-state--pending' }, h('p', { text: why })) });
        }
        const formats = pf.formats || [];
        return card('Page image formats', {
            sub: C.fmtNum(pf.scanned) + ' archives read' + (C.num(pf.pending) ? ', ' + C.fmtNum(pf.pending) + ' still to read' : '') + (C.num(pf.unreadable) ? ', ' + C.fmtNum(pf.unreadable) + ' unreadable (not zip)' : ''),
            body: [
                C.barList(formats.map((f) => ({ key: f.ext, label: String(f.ext).toUpperCase(), value: f.pages, sub: many(f.volumes, 'volume', C.fmtNum) })), { kind: 'count', emptyText: 'No archive read yet.', label: 'Pages by image format' }),
            ],
        });
    }

    function growthCard(data) {
        const growth = data.growth || {};
        return card('Library growth', {
            sub: 'One snapshot per UTC day',
            body: chartsByUnit(growth, data, { type: 'line', label: 'Library growth', height: 160, perSeries: true, granularity: DAILY }),
            wide: true,
        });
    }

    function uploadsCard(data) {
        const up = data.uploads || {};
        const series = up.series || [];
        const perUploader = series.length > 0 && series.every((sr) => kindOf(sr.key) === 'count');
        const chart = perUploader
            ? C.timeSeries({ t: up.t, series: series.map((sr) => ({ key: sr.key, label: sr.label || sr.key, values: sr.values })), granularity: DAILY, type: 'bar', kind: 'count', colorFor: colors.generic, height: 180, label: 'Volumes uploaded', emptyText: 'No uploads in this range.' })
            : chartsByUnit(up, data, { type: 'bar', label: 'Uploads', height: 160, granularity: DAILY });
        const recent = up.recent || [];
        return card('Uploads', {
            sub: 'Volumes added per day, by uploader. Sizes are known for uploads since analytics started.',
            body: [chart, grid([
                C.table([
                    { key: 'username', label: 'Uploader' },
                    { key: 'volumes', label: 'Volumes', kind: 'int' },
                    { key: 'bytes', label: 'Size', num: true, render: (r) => (C.num(r.bytes) ? C.fmtBytes(r.bytes) : '-') },
                ], { rows: up.by_uploader || [], sortKey: 'volumes', emptyText: 'No uploads in this range.', caption: 'Uploads by uploader' }),
                recent.length ? h('div', { class: 'an-sub' }, [
                    h('h4', { class: 'an-sub__title', text: 'Latest uploads' }),
                    C.table([
                        { key: 'volume_key', label: 'Volume', render: (r) => h('span', { class: 'an-title' }, [h('span', { text: String(r.volume_key || '').split('/').pop().replace(/\.cbz$/i, ''), title: r.volume_key }), r.series_title ? h('span', { class: 'an-muted', text: r.series_title }) : null]) },
                        { key: 'username', label: 'By' },
                        { key: 'uploaded_at', label: 'When', num: true, render: (r) => h('span', { text: C.fmtAgo(r.uploaded_at), title: C.fmtDateTime(r.uploaded_at) }) },
                    ], { rows: recent, sortKey: 'uploaded_at', caption: 'Latest uploads' }),
                ]) : null,
            ])],
            wide: true,
        });
    }

    function dormantCard(dormant) {
        if (!dormant) return card('Dormant series', { body: C.emptyState('Not reported.') });
        const since = state.trackingSince && (Date.now() / 1000 - state.trackingSince) < (dormant.days || 90) * 86400
            ? ' Analytics started ' + C.fmtDate(state.trackingSince) + ', so this covers less than ' + (dormant.days || 90) + ' days.' : '';
        return card('Dormant series', {
            sub: 'No download and no read in the last ' + (dormant.days || 90) + ' days.' + since,
            body: C.table([
                { key: 'title', label: 'Series', render: (r) => h('span', { class: 'an-title', text: r.title || r.series_key, title: r.series_key }) },
                { key: 'volumes', label: 'Volumes', kind: 'int' },
            ], { rows: dormant.series || [], sortKey: 'volumes', emptyText: 'Every series was read or downloaded recently.', limit: 200 }),
            wide: true,
        });
    }

    // ------------------------------------------------------------------
    // Clients & geography
    // ------------------------------------------------------------------

    async function renderClients() {
        const [clients, geo] = await Promise.all([tryGet('clients'), tryGet('geo')]);
        const metric = state.clients.metric;
        const kind = metric === 'bytes_out' ? 'bytes' : 'count';
        const parts = [];
        parts.push(panelFor(clients, 'the client mix', (data) => {
            const bars = (rows, dict, title, colorFor) => card(title, {
                body: C.barList((rows || []).map((r) => ({
                    key: r.key, label: named(dict, r.key, r.label), value: r[metric], color: colorFor ? colorFor(r.key) || C.OTHER : undefined,
                    sub: many(r.requests, 'request') + ' · ' + C.fmtBytes(r.bytes_out) + ' · ' + many(r.principals, 'visitor'),
                })).sort((a, b) => C.num(b.value) - C.num(a.value)), { kind, label: title }),
            });
            return stack([
                card('Requests by app', {
                    sub: DAILY_NOTE + ' · the hosted reader is recognised by its Origin; WebDAV clients by their user agent',
                    body: C.timeSeries({ t: tsOf(data, 'by_app').t, series: seriesFrom(tsOf(data, 'by_app'), colors.app, APP), granularity: DAILY, type: 'area', kind: 'count', colorFor: colors.app, label: 'Requests by app' }),
                    wide: true,
                }),
                grid([
                    bars(data.apps, APP, 'Apps', colors.app),
                    bars(data.protocols, PROTOCOL, 'Protocols', colors.protocol),
                    bars(data.clients, CLIENT, 'Clients'),
                    bars(data.browsers, BROWSER, 'Browsers'),
                    bars(data.os, OS, 'Operating systems'),
                    card('Origins', {
                        sub: 'Browser apps calling this server cross-origin',
                        body: C.table([
                            { key: 'origin', label: 'Origin', render: (r) => mono(r.origin) },
                            { key: 'requests', label: 'Requests', kind: 'int' },
                        ], { rows: data.origins || [], sortKey: 'requests', emptyText: 'No cross-origin requests in this range.' }),
                    }),
                ], 'an-grid--3'),
            ]);
        }, () => render()));
        parts.push(geoCard(geo));
        return stack(parts);
    }

    function geoCard(result) {
        const attribution = h('p', { class: 'an-attribution' }, [
            'IP geolocation by ',
            h('a', { href: 'https://db-ip.com', target: '_blank', rel: 'noopener', text: 'DB-IP' }),
            ' (IP to Country Lite, CC BY 4.0)',
        ]);
        if (result.error || (result.data && result.data.available === false)) {
            return card('Geography', { body: [panelFor(result, 'countries', () => null, () => render()), attribution], wide: true });
        }
        const data = result.data;
        // `geo_enabled` is the lookup's own state (analytics and geography both
        // on); `enabled` is analytics as a whole, and stood for it before.
        const geoOn = typeof data.geo_enabled === 'boolean' ? data.geo_enabled : !!data.enabled;
        const refresh = h('button', { type: 'button', class: 'btn btn--secondary btn--sm', text: data.db && data.db.downloaded_at ? 'Refresh database' : 'Download database' });
        refresh.addEventListener('click', async () => {
            refresh.disabled = true;
            try {
                const res = await apiPost('/analytics/geo/refresh', {});
                if (res && res.started === false) {
                    showToast('Geography is off: nothing to download', 'info');
                    refresh.disabled = false;
                    return;
                }
                showToast('Downloading the country database', 'success');
                setTimeout(() => { if (state.view === 'clients') render(); }, 1500);
            } catch (err) {
                showToast('Could not start the download: ' + err.message, 'error');
                refresh.disabled = false;
            }
        });
        const statusText = { off: 'Off', downloading: 'Downloading…', ready: 'Ready', error: 'Error' };
        const tone = { off: 'muted', downloading: 'info', ready: 'success', error: 'error' };
        const status = h('p', { class: 'an-geo-status' }, [
            badge(statusText[data.status] || data.status || 'Unknown', tone[data.status]),
            data.db && data.db.version ? h('span', { class: 'an-muted', text: ' Database ' + data.db.version + (data.db.downloaded_at ? ', downloaded ' + C.fmtDate(data.db.downloaded_at) : '') }) : null,
        ]);
        const body = [status];
        if (data.status === 'error' && data.error) body.push(h('p', { class: 'an-note an-note--bad', role: 'alert', text: data.error }));
        if (!geoOn) {
            body.push(h('div', { class: 'an-state an-state--pending' }, [
                h('p', { class: 'an-state__title', text: 'Geography is off' }),
                h('p', { text: 'Turn it on in Analytics settings to look up a country for each client IP. The server then downloads the free DB-IP country database (a few MB) once a month; lookups stay on this server.' }),
                h('p', null, h('a', { href: viewHref('settings'), text: 'Open Analytics settings', onclick: (e) => { e.preventDefault(); setView('settings'); } })),
            ]));
        } else {
            const countries = (data.countries || []).slice().sort((a, b) => C.num(b.requests) - C.num(a.requests));
            body.push(grid([
                C.barList(countries.map((c) => ({ key: c.country, label: countryName(c.country), value: c.requests, sub: C.fmtBytes(c.bytes_out) + ' sent' })), { kind: 'count', limit: 12, label: 'Requests by country', emptyText: data.status === 'ready' ? 'No traffic in this range.' : 'No country data yet.' }),
                C.table([
                    { key: 'country', label: 'Country', render: (r) => countryName(r.country), sortValue: (r) => countryName(r.country) },
                    { key: 'requests', label: 'Requests', kind: 'int' },
                    { key: 'bytes_out', label: 'Sent', kind: 'bytes' },
                ], { rows: countries, sortKey: 'requests', emptyText: 'No country data yet.' }),
            ]));
        }
        body.push(attribution);
        return card('Geography', {
            sub: 'Rough location of client IPs (country only)',
            actions: geoOn ? [refresh] : null,
            body,
            wide: true,
        });
    }

    // ------------------------------------------------------------------
    // Health
    // ------------------------------------------------------------------

    async function renderHealth() {
        const data = await get('health');
        if (data.available === false) return notCollected('request health');
        const proto = state.health.protocol;
        const only = (rows) => (rows || []).filter((r) => !proto || r.protocol === proto);
        const protoName = proto ? named(PROTOCOL, proto) : '';
        const eff = data.efficiency || {};
        const gzipSaved = Math.max(0, C.num(eff.gzip_raw_bytes) - C.num(eff.gzip_sent_bytes));
        const af = data.auth_failures || {};
        const statusTs = data.status && typeof data.status === 'object' && Array.isArray(data.status.series) ? data.status : { t: data.t, series: data.series };
        // The server labels its classes "2xx"..."5xx"; these say what they are.
        const STATUS = { '2xx': '2xx success', '3xx': '3xx redirect / not modified', '4xx': '4xx client error', '5xx': '5xx server error' };
        const statusSeries = seriesFrom(statusTs, colors.status, STATUS).map((sr) => Object.assign(sr, { label: STATUS[sr.key] || sr.label }));
        return stack([
            card('Responses by status', {
                sub: granularityNote(data) + ' · all protocols',
                body: C.timeSeries({ t: statusTs.t || data.t, series: statusSeries, granularity: gran(data), type: 'area', kind: 'count', colorFor: colors.status, label: 'Responses by status class' }),
                wide: true,
            }),
            card('Transfer efficiency', {
                sub: rangeLong(),
                body: C.kpis([
                    { label: 'Not modified (304)', value: C.fmtNum(eff.not_modified), sub: C.fmtBytes(eff.saved_bytes) + ' not resent' },
                    { label: 'Gzip savings', value: C.fmtBytes(gzipSaved), sub: C.num(eff.gzip_raw_bytes) ? C.fmtRatio(gzipSaved / C.num(eff.gzip_raw_bytes)) + ' of ' + C.fmtBytes(eff.gzip_raw_bytes) + ' listings' : 'no compressed listings' },
                    { label: 'Aborted downloads', value: C.fmtNum(eff.aborted), sub: C.fmtBytes(eff.aborted_bytes) + ' sent before the abort' },
                    { label: 'Resumed downloads', value: C.fmtNum(eff.resumed), sub: 'range requests on volumes' },
                    { label: 'Sent by nginx (estimated)', value: C.fmtBytes(eff.est_bytes_out), sub: C.num(eff.est_bytes_out) ? 'X-Accel-Redirect offload; aborts invisible' : 'offload not in use' },
                ]),
                wide: true,
            }),
            card('Latency' + (protoName ? ': ' + protoName : ''), {
                sub: 'Time to the last body byte (p50 / p95 / p99) and to the response headers, per request kind',
                body: C.table([
                    { key: 'protocol', label: 'Protocol', render: (r) => named(PROTOCOL, r.protocol) },
                    { key: 'method', label: 'Method', render: (r) => mono(r.method) },
                    { key: 'category', label: 'Category', render: (r) => named(CATEGORY, r.category) },
                    { key: 'requests', label: 'Requests', kind: 'int' },
                    { key: 'p50_ms', label: 'p50', kind: 'ms' },
                    { key: 'p95_ms', label: 'p95', kind: 'ms' },
                    { key: 'p99_ms', label: 'p99', kind: 'ms' },
                    { key: 'ttfb_p50_ms', label: 'Headers p50', kind: 'ms' },
                    { key: 'ttfb_p95_ms', label: 'Headers p95', kind: 'ms' },
                ], { rows: only(data.latency), sortKey: 'requests', emptyText: proto ? 'No ' + protoName + ' requests in this range.' : 'No requests in this range.' }),
                wide: true,
            }),
            grid([
                card('Errors' + (protoName ? ': ' + protoName : ''), {
                    sub: 'Responses with status 400 and up',
                    body: C.table([
                        { key: 'status', label: 'Status', render: (r) => badge(String(r.status), C.num(r.status) >= 500 ? 'error' : 'warning') },
                        { key: 'protocol', label: 'Protocol', render: (r) => named(PROTOCOL, r.protocol) },
                        { key: 'method', label: 'Method', render: (r) => mono(r.method) },
                        { key: 'category', label: 'Category', render: (r) => named(CATEGORY, r.category) },
                        { key: 'count', label: 'Count', kind: 'int' },
                    ], { rows: only(data.errors), sortKey: 'count', emptyText: 'No errors in this range.' }),
                }),
                card('Failed sign-ins', {
                    sub: 'Wrong passwords and tokens, over WebDAV Basic auth and the login page alike',
                    body: [
                        C.timeSeries({ t: af.t || data.t, series: [{ key: 'auth_failures', label: 'Failed sign-ins', values: af.values || (af.series && af.series[0] && af.series[0].values) || [], color: 'var(--an-s1)' }], granularity: gran(data), type: 'bar', kind: 'count', height: 140, label: 'Failed sign-ins', emptyText: 'No failed sign-ins in this range.' }),
                        C.table([
                            { key: 'ip', label: 'IP', render: (r) => mono(r.ip) },
                            { key: 'country', label: 'Country', render: (r) => (r.country ? countryName(r.country) : '-') },
                            { key: 'count', label: 'Failures', kind: 'int' },
                            { key: 'usernames', label: 'Tried', sortable: false, render: (r) => (r.usernames && r.usernames.length ? r.usernames.slice(0, 4).join(', ') + (r.usernames.length > 4 ? ', …' : '') : '-') },
                        ], { rows: af.top_ips || [], sortKey: 'count', emptyText: 'No failed sign-ins in this range.' }),
                        (af.top_usernames || []).length ? h('div', { class: 'an-sub' }, [
                            h('h4', { class: 'an-sub__title', text: 'Usernames tried' }),
                            C.barList(af.top_usernames.map((u) => ({ key: u.username, label: u.username || '(no username)', value: u.count })), { kind: 'count', label: 'Usernames tried', limit: 8 }),
                        ]) : null,
                    ],
                }),
            ]),
            card('Slowest requests', {
                sub: 'The slowest few per hour (kept 7 days)',
                body: C.table([
                    { key: 'at', label: 'When', num: true, render: (r) => C.fmtDateTime(r.at) },
                    { key: 'method', label: 'Method', render: (r) => mono(r.method) },
                    { key: 'path', label: 'Path', className: 'an-col-path', render: (r) => mono(r.path) },
                    { key: 'status', label: 'Status', kind: 'int' },
                    { key: 'ms', label: 'Time', kind: 'ms' },
                    { key: 'principal', label: 'Who', render: (r) => mono(r.principal) },
                ], { rows: (data.slow || []).filter((r) => !proto || slowProtocol(r) === proto), sortKey: 'ms', emptyText: 'Nothing slow in this range.' }),
                wide: true,
            }),
        ]);
    }

    const DAV_ONLY = ['PROPFIND', 'PROPPATCH', 'MKCOL', 'COPY', 'MOVE', 'LOCK', 'UNLOCK'];
    // First path segments the server's own pages and APIs are routed under.
    // Anything else (library folders, series.json, covers, archives,
    // /mokuro-reader, /inbox) reaches the WebDAV fallback.
    const WEB_ROOTS = ['_static', '_home', '_admin', 'catalog', 'login', 'register', 'account', 'setup', 'queue'];
    // Slow rows carry no protocol (section 4.4): read it off the method and
    // the path, as the server's classifier does (section 4.3). Paths are as
    // the router sees them, so a sub-path mount's prefix is already gone.
    function slowProtocol(row) {
        if (row.protocol) return row.protocol;
        const method = String(row.method || '').toUpperCase();
        const path = String(row.path || '').split('?')[0];
        const segs = path.replace(/^\/+/, '').split('/');
        const first = segs[0];
        const page = method === 'GET' || method === 'HEAD';
        if (String(row.principal || '').startsWith('p:') || first === '_processor') return 'processor';
        if (DAV_ONLY.includes(method)) return 'dav';
        if (first === 'api' || (WEB_ROOTS.includes(first) && segs[1] === 'api') || (first === 'setup' && segs[1] === 'code' && segs.length === 2)) return 'api';
        // `/` is WebDAV's too; only a browser's GET of it is the home page.
        if (first === '' && segs.length === 1) return page ? 'web' : 'dav';
        if (first === 'robots.txt' && segs.length === 1 && page) return 'web';
        // The admin shell takes any method; the other pages only GET (a PUT
        // to /login/x falls through to WebDAV, as in 0.5.2).
        if (first === '_admin' || (WEB_ROOTS.includes(first) && page)) return 'web';
        return 'dav';
    }

    // ------------------------------------------------------------------
    // Server (host)
    // ------------------------------------------------------------------

    async function renderServer() {
        const range = HOST_RANGES.includes(state.range) ? state.range : '30d';
        const data = await get('host', { range });
        if (data.available === false) return notCollected('memory and CPU readings');
        const now = data.now || {};
        const ps = data.platform_supported || {};
        const find = (key) => (data.series || []).find((sr) => sr.key === key);
        const lines = (keys, labels) => keys.map((k, i) => find(k) && { key: k, label: labels[i], values: find(k).values, color: C.slotColor(i + 1) }).filter(Boolean);
        const g = gran(data);
        // 24h is read per minute, 7d and 30d per hour (`resolution`); both are
        // in the viewer's local time. A null is no reading (the server was
        // down): the lines break there.
        const perMinute = data.resolution === 'minute';
        const note = perMinute ? 'Per minute, in your local time' : granularityNote(data);
        const known = (v) => v !== null && v !== undefined;
        const notes = [];
        if (range !== state.range) notes.push(h('p', { class: 'an-note', text: 'Server readings go back 30 days at most here; showing the last 30 days.' }));
        if (ps.rss === false) notes.push(h('p', { class: 'an-note', text: 'This platform does not report process memory.' }));
        if (ps.cpu === false) notes.push(h('p', { class: 'an-note', text: 'This platform does not report CPU time.' }));
        const lost = C.num(now.reading_dropped) + C.num(now.collector_dropped);
        if (lost > 0) notes.push(h('p', { class: 'an-note an-note--warn', text: 'Under load, ' + C.fmtNum(lost) + ' events went to overflow counters' + (C.num(now.reading_dropped) ? ' (' + C.fmtNum(now.reading_dropped) + ' synced-file notices were dropped; those files are read on their next write)' : '') + '.' }));
        // Host memory needs both numbers (macOS reports no cheap "available").
        const hostKnown = C.num(now.host_total) > 0 && known(now.host_available);
        const hostMem = hostKnown ? C.segmented([
            { key: 'used', label: 'In use', value: C.num(now.host_total) - C.num(now.host_available), color: 'var(--an-s1)' },
            { key: 'available', label: 'Available', value: now.host_available, color: 'var(--an-track)' },
        ], { kind: 'bytes', label: 'Host memory' }) : null;
        const cpuNow = ps.cpu === false ? 'n/a' : known(now.cpu_pct) ? C.fmtPct(now.cpu_pct) : '-';
        return stack(notes.concat([
            C.kpis([
                { label: 'Memory (RSS)', value: ps.rss === false || !known(now.rss) ? 'n/a' : C.fmtBytes(now.rss), sub: ps.rss === false || !known(now.peak_rss) ? null : 'peak ' + C.fmtBytes(now.peak_rss) },
                { label: 'CPU', value: cpuNow, sub: ps.cpu !== false && !known(now.cpu_pct) ? 'measuring (needs two samples)' : 'of one core' },
                { label: 'Connections', value: C.fmtNum(now.connections), sub: 'open now' },
                { label: 'In flight', value: C.fmtNum(now.inflight), sub: 'requests being answered' },
                hostKnown ? { label: 'Host memory', value: C.fmtBytes(now.host_available) + ' free', sub: 'of ' + C.fmtBytes(now.host_total) } : null,
            ]),
            hostMem ? card('Host memory', { body: hostMem }) : null,
            card('Process memory', {
                sub: note,
                body: C.timeSeries({ t: data.t, series: lines(['rss', 'rss_max'], ['Average RSS', 'Highest RSS']), granularity: g, type: 'line', kind: 'bytes', gaps: true, label: 'Process memory', emptyText: 'No memory readings in this range.' }),
                wide: true,
            }),
            grid([
                card('CPU', {
                    sub: 'Percent of one core · ' + note,
                    // Per minute the highest is the reading itself.
                    body: C.timeSeries({ t: data.t, series: perMinute ? lines(['cpu_pct'], ['CPU']) : lines(['cpu_pct', 'cpu_max'], ['Average CPU', 'Highest CPU']), granularity: g, type: 'line', kind: 'pct', gaps: true, height: 180, label: 'CPU', emptyText: 'No CPU readings in this range.' }),
                }),
                card('Connections and requests in flight', {
                    sub: 'Highest per ' + (perMinute ? 'minute' : 'hour'),
                    body: C.timeSeries({ t: data.t, series: lines(['connections_max', 'inflight_max'], ['Connections', 'In flight']), granularity: g, type: 'line', kind: 'count', gaps: true, height: 180, label: 'Connections and requests in flight', emptyText: 'No readings in this range.' }),
                }),
            ]),
        ]).filter(Boolean));
    }

    // ------------------------------------------------------------------
    // Crawlers
    // ------------------------------------------------------------------

    async function renderCrawlers() {
        const data = await get('crawlers');
        if (data.available === false) return notCollected('crawl episodes');
        const th = data.thresholds || {};
        const episodes = data.episodes || [];
        const open = episodes.filter((e) => e.status === 'open').length;
        const perDayTs = tsOf(data, 'episodes_per_day');
        const perDay = perDayTs.series[0] || { values: [] };
        const t = perDayTs.t;
        // Both thresholds must hold, so the series an IP needs are the larger
        // of min_series and the coverage share of the catalog.
        const minSeries = Math.max(1, C.num(th.min_series === undefined ? 20 : th.min_series));
        const coverage = th.min_coverage === undefined ? 0.25 : C.num(th.min_coverage);
        const catalog = C.num(data.catalog_series);
        const needed = catalog ? Math.max(minSeries, Math.ceil(catalog * coverage - 1e-9)) : minSeries;
        const rule = 'An episode opens when one IP touches at least ' + C.fmtNum(minSeries) + ' series that are also at least ' +
            C.fmtRatio(coverage) + ' of the catalog' +
            (catalog ? ': ' + C.fmtNum(needed) + ' of the ' + C.fmtNum(catalog) + ' series here' + (needed > catalog ? ', more than there are, so none can open' : '') : '') +
            ', within ' + C.fmtNum(th.window_hours || 6) + ' hours, by downloading volumes or by asking about one series at a time (WebDAV folder listings of a series, the catalog\'s series pages and manifests). ' +
            'The reader\'s own sync (its one whole-library listing, series.json refreshes, covers) is no signal. Detected and reported only: nothing is blocked or slowed.';
        return stack([
            h('p', { class: 'an-note', text: rule }),
            C.kpis([
                { label: 'Episodes', value: C.fmtNum(episodes.length), sub: rangeLong() },
                { label: 'Open now', value: C.fmtNum(open), tone: open ? 'warn' : null },
                { label: 'Distinct IPs', value: C.fmtNum((data.by_ip || []).length) },
                { label: 'Sent to crawlers', value: C.fmtBytes(episodes.reduce((a, e) => a + C.num(e.bytes_out), 0)) },
            ]),
            card('Episodes per day', {
                sub: 'By UTC day',
                body: C.timeSeries({ t, series: [{ key: 'episodes', label: 'Episodes', values: perDay.values, color: 'var(--an-s1)' }], granularity: 'day', type: 'bar', kind: 'count', height: 150, label: 'Crawl episodes per day', emptyText: 'No crawl episodes in this range.' }),
                wide: true,
            }),
            card('Episodes', {
                sub: 'Newest first. Admins are not exempt; the visitor column shows who it was.',
                body: C.table([
                    { key: 'status', label: 'State', render: (r) => badge(r.status === 'open' ? 'Open' : 'Ended', r.status === 'open' ? 'warning' : 'muted') },
                    { key: 'ip', label: 'IP', className: 'an-col-ip', render: (r) => h('span', { class: 'an-title' }, [mono(r.ip), r.country ? h('span', { class: 'an-muted', text: countryName(r.country) }) : null]) },
                    { key: 'principals', label: 'Visitor', sortable: false, render: (r) => ((r.principals || []).length ? h('span', { class: 'an-tags' }, r.principals.slice(0, 3).map((p) => principalCell({ principal: p, label: String(p).startsWith('u:') ? String(p).slice(2) : p }))) : '-') },
                    { key: 'client', label: 'Client', render: (r) => h('span', { text: named(CLIENT, r.client), title: r.user_agent || '' }) },
                    { key: 'kind', label: 'Kind', render: (r) => ({ download: 'Downloads', metadata: 'Metadata', mixed: 'Mixed' }[r.kind] || r.kind || '-') },
                    { key: 'started_at', label: 'Started', num: true, render: (r) => C.fmtDateTime(r.started_at) },
                    { key: 'last_at', label: 'Last seen', num: true, render: (r) => h('span', { text: C.fmtAgo(r.last_at), title: C.fmtDateTime(r.last_at) }) },
                    { key: 'series', label: 'Series', kind: 'int' },
                    { key: 'volumes', label: 'Volumes', kind: 'int' },
                    { key: 'coverage', label: 'Coverage', kind: 'ratio' },
                    { key: 'bytes_out', label: 'Sent', kind: 'bytes' },
                    { key: 'requests', label: 'Requests', kind: 'int' },
                ], { rows: episodes, sortKey: 'last_at', emptyText: 'No crawl episodes in this range.' }),
                wide: true,
            }),
            card('By IP', {
                sub: 'How often each IP crawled',
                body: C.table([
                    { key: 'ip', label: 'IP', className: 'an-col-ip', render: (r) => mono(r.ip) },
                    { key: 'country', label: 'Country', render: (r) => (r.country ? countryName(r.country) : '-') },
                    { key: 'episodes', label: 'Episodes', kind: 'int' },
                    { key: 'last_at', label: 'Last', num: true, render: (r) => h('span', { text: C.fmtAgo(r.last_at), title: C.fmtDateTime(r.last_at) }) },
                    { key: 'bytes_out', label: 'Sent', kind: 'bytes' },
                ], { rows: data.by_ip || [], sortKey: 'episodes', emptyText: 'No crawling IPs in this range.' }),
            }),
        ]);
    }

    // ------------------------------------------------------------------
    // Settings: the `analytics:` config section
    // ------------------------------------------------------------------

    const SETTINGS_DEFAULTS = {
        enabled: true,
        flush_seconds: 60,
        retention: { ip_days: 30, hourly_days: 90, host_minute_days: 7, crawl_days: 90 },
        crawl: { window_hours: 6, min_series: 20, min_coverage: 0.25 },
        library_scan: { interval_hours: 6, page_formats: true },
        geo: { enabled: false, url: '' },
    };

    async function renderSettings() {
        const settings = await apiGet('/settings');
        const cfg = settings.analytics;
        if (!cfg || typeof cfg !== 'object') {
            return h('div', { class: 'an-state an-state--pending' }, [
                h('p', { class: 'an-state__title', text: 'Not available' }),
                h('p', { text: 'This server does not report analytics settings. Edit the analytics: section of config.yaml instead.' }),
            ]);
        }
        const v = merge(SETTINGS_DEFAULTS, cfg);
        if (typeof v.enabled === 'boolean') state.enabled = v.enabled;
        drawBanner();
        const form = h('form', { class: 'an-settings', novalidate: true });
        const fields = {};
        const field = (path, label, control, hint) => {
            const id = 'an-set-' + path.replace(/\./g, '-');
            control.id = id;
            control.dataset.path = path;
            fields[path] = control;
            const isCheck = control.type === 'checkbox';
            const labelNode = isCheck ? h('span', { class: 'field__label', text: label }) : h('label', { class: 'field__label', for: id, text: label });
            const ctl = isCheck ? h('label', { class: 'check', for: id }, [control, ' ' + hint]) : control;
            const err = h('p', { class: 'form-error an-field-error', id: id + '-error', hidden: true });
            control.setAttribute('aria-describedby', id + '-error');
            return h('div', { class: 'field' }, [labelNode, h('div', { class: 'field__control' }, [ctl, !isCheck && hint ? h('p', { class: 'form-hint', text: hint }) : null, err])]);
        };
        // Bounds are AnalyticsConfig::validate's; `above` makes the minimum
        // exclusive (a share must be above 0).
        const numInput = (value, min, max, step, above) => h('input', { type: 'number', class: 'form-input an-num', value: String(value), min, max, step: step || 1, inputmode: step && step < 1 ? 'decimal' : 'numeric', 'data-above': above ? 'true' : null });
        const check = (value) => h('input', { type: 'checkbox', checked: !!value });
        const section = (title, desc, rows) => h('section', { class: 'settings-section' }, [
            h('header', { class: 'settings-section__header' }, [h('h3', { text: title }), desc ? h('p', { text: desc }) : null]),
        ].concat(rows));

        form.appendChild(section('Recording', 'Counts the body bytes, status and timing of every request, WebDAV and web alike, in analytics.sqlite beside the main database. Delete that file to start over.', [
            field('enabled', 'Analytics', check(v.enabled), 'Record traffic, downloads, reading and server health'),
            field('flush_seconds', 'Write to disk every', numInput(v.flush_seconds, 5, 3600), 'Seconds. A crash loses at most this much.'),
        ]));
        form.appendChild(section('Retention', 'Daily totals are kept forever; these say how long the detail stays.', [
            field('retention.ip_days', 'Client IPs', numInput(v.retention.ip_days, 1, 3650), 'Days. Rows naming a raw IP (per-IP traffic, anonymous visitor IPs, failed sign-ins, slow requests); after that only pseudonymous ids remain.'),
            field('retention.hourly_days', 'Hourly detail', numInput(v.retention.hourly_days, 1, 3650), 'Days of hourly traffic and latency.'),
            field('retention.host_minute_days', 'Per-minute server readings', numInput(v.retention.host_minute_days, 1, 365), 'Days.'),
            field('retention.crawl_days', 'Crawl episodes', numInput(v.retention.crawl_days, 1, 3650), 'Days.'),
        ]));
        form.appendChild(section('Crawl detection', 'Reported only: a crawler is never blocked or slowed.', [
            field('crawl.window_hours', 'Window', numInput(v.crawl.window_hours, 1, 168), 'Hours an IP\'s activity is gathered over.'),
            field('crawl.min_series', 'At least', numInput(v.crawl.min_series, 1, 1000000), 'Distinct series touched in the window (downloads, or listings and catalog pages of one series).'),
            field('crawl.min_coverage', 'And at least', numInput(v.crawl.min_coverage, 0, 1, 0.01, true), 'Share of the catalog, above 0 and at most 1 (0.25 = a quarter of all series).'),
        ]));
        form.appendChild(section('Library scan', 'File types, page formats and growth.', [
            field('library_scan.interval_hours', 'Scan every', numInput(v.library_scan.interval_hours, 1, 720), 'Hours.'),
            field('library_scan.page_formats', 'Page formats', check(v.library_scan.page_formats), 'Read each archive\'s zip directory to count page image formats (local libraries only)'),
        ]));
        form.appendChild(section('Geography', 'Country per client IP, from the free DB-IP country database. Off by default.', [
            field('geo.enabled', 'Countries', check(v.geo.enabled), 'Download the DB-IP IP to Country Lite database (CC BY 4.0) and look up client IPs on this server'),
            field('geo.url', 'Database URL', h('input', { type: 'url', class: 'form-input', value: v.geo.url || '', spellcheck: 'false' }), '{year} and {month} are filled in; the previous month is tried when this month\'s is not out yet.'),
        ]));
        const status = h('p', { class: 'form-hint', role: 'status' });
        const save = h('button', { type: 'submit', class: 'btn btn--primary', text: 'Save analytics settings' });
        form.appendChild(h('footer', { class: 'settings-section__footer' }, [save, status]));
        form.addEventListener('submit', async (e) => {
            e.preventDefault();
            form.querySelectorAll('.an-field-error').forEach((p) => { p.hidden = true; p.textContent = ''; });
            form.querySelectorAll('[aria-invalid]').forEach((n) => n.removeAttribute('aria-invalid'));
            const body = readSettings(fields);
            if (!body) return;
            save.disabled = true;
            status.textContent = 'Saving…';
            try {
                const result = await apiPut('/settings/analytics', body);
                const saved = result && result.analytics;
                if (saved && typeof saved.enabled === 'boolean') state.enabled = saved.enabled;
                else state.enabled = body.enabled;
                drawBanner();
                status.textContent = 'Saved.';
                showToast('Analytics settings saved', 'success');
            } catch (err) {
                status.textContent = '';
                // The server names the field first: "analytics.crawl.window_hours: ...".
                const named = /^analytics\.([a-z_.]+):/.exec(String(err.message || ''));
                const path = (err.payload && err.payload.field) || (named && named[1]);
                const target = path && (fields[path] || fields[String(path).replace(/^analytics\./, '')]);
                if (target) showFieldError(target, err.message);
                showToast(err.message, 'error');
            } finally {
                save.disabled = false;
            }
        });
        return h('div', { class: 'settings-body an-settings-wrap' }, form);
    }

    function showFieldError(control, message) {
        control.setAttribute('aria-invalid', 'true');
        const err = document.getElementById(control.id + '-error');
        if (err) {
            err.textContent = message;
            err.hidden = false;
        }
        control.focus();
    }

    // The whole section, in the config's own shape (section 11 of the
    // design): numbers as numbers, every key present.
    function readSettings(fields) {
        const out = {};
        let bad = null;
        Object.keys(fields).forEach((path) => {
            const control = fields[path];
            let value;
            if (control.type === 'checkbox') value = control.checked;
            else if (control.type === 'number') {
                value = Number(control.value);
                const min = Number(control.min);
                const max = Number(control.max);
                const above = control.dataset.above === 'true';
                if (control.value.trim() === '' || !Number.isFinite(value) || (above ? value <= min : value < min) || value > max) {
                    if (!bad) bad = [control, above ? 'Enter a number above ' + control.min + ' and at most ' + control.max + '.' : 'Enter a number from ' + control.min + ' to ' + control.max + '.'];
                    return;
                }
                if (Number(control.step || 1) >= 1 && !Number.isInteger(value)) {
                    if (!bad) bad = [control, 'Enter a whole number.'];
                    return;
                }
            } else value = control.value.trim();
            const keys = path.split('.');
            let node = out;
            keys.slice(0, -1).forEach((k) => { node = node[k] = node[k] || {}; });
            node[keys[keys.length - 1]] = value;
        });
        if (bad) {
            showFieldError(bad[0], bad[1]);
            return null;
        }
        return out;
    }

    function merge(defaults, value) {
        const out = {};
        Object.keys(defaults).forEach((k) => {
            const d = defaults[k];
            const v = value ? value[k] : undefined;
            out[k] = d && typeof d === 'object' && !Array.isArray(d) ? merge(d, v && typeof v === 'object' ? v : {}) : (v === undefined || v === null ? d : v);
        });
        return out;
    }

    const RENDER = {
        overview: renderOverview,
        traffic: renderTraffic,
        popular: renderPopular,
        reading: renderReading,
        users: renderUsers,
        library: renderLibrary,
        clients: renderClients,
        health: renderHealth,
        server: renderServer,
        crawlers: renderCrawlers,
        settings: renderSettings,
    };

    // ------------------------------------------------------------------
    // Tab lifecycle (admin.js initTabs calls these)
    // ------------------------------------------------------------------

    window.bunkoAnalytics = {
        open() {
            build();
            if (!state.ready) return;
            readHash();
            state.active = true;
            state.renderedView = null;
            render();
        },
        close() {
            if (!state.active) return;
            state.active = false;
            state.seq += 1;
            if ((window.location.hash || '').startsWith('#analytics')) {
                try { history.replaceState(null, '', window.location.pathname + window.location.search); } catch (_) { /* ignore */ }
            }
        },
    };
})();
