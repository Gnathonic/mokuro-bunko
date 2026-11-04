// Analytics charts: small, dependency-free inline SVG and HTML figures for the
// admin panel's Analytics tab. Everything here is drawn from data the server
// sends, so every label goes into the page as text (textContent), never as
// markup. Colours are CSS custom properties (analytics.css), so the marks
// follow the panel's theme from one place.
(function () {
    'use strict';

    const SVGNS = 'http://www.w3.org/2000/svg';

    // ------------------------------------------------------------------
    // DOM helpers
    // ------------------------------------------------------------------

    // h('div', {class: 'x', text: 'label'}, [children]) -- `text` is set as
    // textContent; attributes are set with setAttribute; on* are listeners.
    function h(tag, attrs, children) {
        const node = document.createElement(tag);
        applyAttrs(node, attrs);
        appendAll(node, children);
        return node;
    }

    function s(tag, attrs, children) {
        const node = document.createElementNS(SVGNS, tag);
        applyAttrs(node, attrs);
        appendAll(node, children);
        return node;
    }

    function applyAttrs(node, attrs) {
        if (!attrs) return;
        Object.keys(attrs).forEach((key) => {
            const value = attrs[key];
            if (value === undefined || value === null || value === false) return;
            if (key === 'text') node.textContent = String(value);
            else if (key.startsWith('on') && typeof value === 'function') node.addEventListener(key.slice(2), value);
            else node.setAttribute(key, value === true ? '' : String(value));
        });
    }

    function appendAll(node, children) {
        if (children === undefined || children === null) return;
        (Array.isArray(children) ? children : [children]).forEach((child) => {
            if (child === undefined || child === null || child === false) return;
            node.appendChild(typeof child === 'string' || typeof child === 'number'
                ? document.createTextNode(String(child)) : child);
        });
    }

    function clear(node) {
        while (node.firstChild) node.removeChild(node.firstChild);
        return node;
    }

    let uid = 0;
    function nextId(prefix) {
        uid += 1;
        return (prefix || 'bc') + '-' + uid;
    }

    // ------------------------------------------------------------------
    // Formatting
    // ------------------------------------------------------------------

    const BYTE_UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
    const intFmt = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
    const compactFmt = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });
    const oneDp = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });

    function num(v) {
        const n = Number(v);
        return Number.isFinite(n) ? n : 0;
    }

    // Binary units: 1 KiB = 1024 B. One decimal below 100 of a unit.
    function fmtBytes(value) {
        let v = num(value);
        const neg = v < 0;
        v = Math.abs(v);
        let i = 0;
        while (v >= 1024 && i < BYTE_UNITS.length - 1) {
            v /= 1024;
            i += 1;
        }
        const text = i === 0 ? intFmt.format(v) : (v < 100 ? oneDp.format(v) : intFmt.format(v));
        return (neg ? '-' : '') + text + ' ' + BYTE_UNITS[i];
    }

    // Full under 10,000; compact (12.9K) from there.
    function fmtNum(value) {
        const v = num(value);
        if (Math.abs(v) < 10000) return Number.isInteger(v) ? intFmt.format(v) : oneDp.format(v);
        return compactFmt.format(v);
    }

    function fmtInt(value) {
        return intFmt.format(num(value));
    }

    // A ratio (0..1) as a percentage.
    function fmtRatio(value) {
        const v = num(value) * 100;
        return (v > 0 && v < 1 ? '<1' : (v < 10 && v % 1 ? oneDp.format(v) : intFmt.format(v))) + '%';
    }

    // A percentage that already is one (cpu_pct).
    function fmtPct(value) {
        const v = num(value);
        return (v < 10 ? oneDp.format(v) : intFmt.format(v)) + '%';
    }

    function fmtMs(value) {
        const v = num(value);
        if (v < 1) return '<1 ms';
        if (v < 1000) return intFmt.format(v) + ' ms';
        if (v < 60000) return oneDp.format(v / 1000) + ' s';
        const m = Math.floor(v / 60000);
        return m + ' min ' + Math.round((v % 60000) / 1000) + ' s';
    }

    function fmtMinutes(value) {
        const v = num(value);
        if (v < 120) return intFmt.format(v) + ' min';
        return oneDp.format(v / 60) + ' h';
    }

    function fmtDuration(seconds) {
        const v = Math.max(0, Math.floor(num(seconds)));
        const d = Math.floor(v / 86400);
        const hr = Math.floor((v % 86400) / 3600);
        const m = Math.floor((v % 3600) / 60);
        if (d > 0) return d + 'd ' + hr + 'h';
        if (hr > 0) return hr + 'h ' + m + 'm';
        return m + 'm';
    }

    const FORMATTERS = {
        bytes: fmtBytes,
        count: fmtNum,
        int: fmtInt,
        ratio: fmtRatio,
        pct: fmtPct,
        ms: fmtMs,
        minutes: fmtMinutes,
    };

    function formatter(kind) {
        return typeof kind === 'function' ? kind : (FORMATTERS[kind] || fmtNum);
    }

    // Hourly points are read in the viewer's own time; daily points are UTC
    // days (the server buckets by UTC day), labelled as dates.
    const hourFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
    const hourOnlyFmt = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
    const dayFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' });
    const dayYearFmt = new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric', timeZone: 'UTC' });
    const dateTimeFmt = new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
    const dateFmt = new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric' });

    function fmtTime(epoch, granularity, long) {
        const d = new Date(num(epoch) * 1000);
        if (granularity === 'hour') return hourFmt.format(d);
        return (long ? dayYearFmt : dayFmt).format(d) + (long ? ' (UTC)' : '');
    }

    function fmtTick(epoch, granularity, spanSeconds) {
        const d = new Date(num(epoch) * 1000);
        if (granularity === 'hour') return spanSeconds <= 86400 * 1.1 ? hourOnlyFmt.format(d) : dayFmtLocal(d);
        return spanSeconds > 366 * 86400 ? dayYearFmt.format(d) : dayFmt.format(d);
    }

    const dayLocalFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });
    function dayFmtLocal(d) {
        return dayLocalFmt.format(d);
    }

    function fmtDateTime(epoch) {
        if (!epoch) return '-';
        return dateTimeFmt.format(new Date(num(epoch) * 1000));
    }

    function fmtDate(epoch) {
        if (!epoch) return '-';
        return dateFmt.format(new Date(num(epoch) * 1000));
    }

    function fmtAgo(epoch) {
        if (!epoch) return 'never';
        const diff = Date.now() / 1000 - num(epoch);
        if (diff < 60) return 'just now';
        if (diff < 3600) return Math.floor(diff / 60) + ' min ago';
        if (diff < 86400) return Math.floor(diff / 3600) + ' h ago';
        if (diff < 86400 * 60) return Math.floor(diff / 86400) + ' d ago';
        return fmtDate(epoch);
    }

    // ------------------------------------------------------------------
    // Colour: identity follows the entity, never its rank
    // ------------------------------------------------------------------

    const SLOTS = 8;
    const OTHER = 'var(--an-other)';

    function slotColor(slot) {
        return 'var(--an-s' + slot + ')';
    }

    // A scale for one domain (categories, methods, apps, ...): keys listed in
    // `fixed` always wear their slot; others take the next free slot the
    // first time they are seen and keep it for the session. Past eight there
    // is no ninth hue: the key folds into Other.
    const registries = {};
    function colorScale(domain, fixed) {
        if (!registries[domain]) {
            const taken = {};
            Object.keys(fixed || {}).forEach((k) => { taken[k] = fixed[k]; });
            registries[domain] = { map: taken, fixedOnly: !!(fixed && fixed.__only) };
        }
        const reg = registries[domain];
        return function (key) {
            if (key === 'other' || key === '(other)') return OTHER;
            if (reg.map[key] !== undefined) return reg.map[key] ? (reg.map[key].startsWith ? reg.map[key] : slotColor(reg.map[key])) : null;
            if (reg.fixedOnly) return null;
            const used = new Set(Object.values(reg.map).filter((v) => typeof v === 'number'));
            for (let i = 1; i <= SLOTS; i += 1) {
                if (!used.has(i)) {
                    reg.map[key] = i;
                    return slotColor(i);
                }
            }
            reg.map[key] = 0;
            return null;
        };
    }

    // Series without a colour (past the palette, or the server's own
    // "other") and series past `max` fold into one Other series.
    function foldSeries(series, colorFor, max) {
        const totals = series.map((sr) => (sr.values || []).reduce((a, b) => a + num(b), 0));
        const order = series.map((_, i) => i).sort((a, b) => totals[b] - totals[a]);
        const keep = new Set();
        order.forEach((i) => {
            const sr = series[i];
            if (keep.size >= max) return;
            if (sr.key === 'other' || !colorFor(sr.key)) return;
            keep.add(i);
        });
        const out = [];
        let other = null;
        const folded = [];
        series.forEach((sr, i) => {
            if (keep.has(i)) {
                out.push({ key: sr.key, label: sr.label || sr.key, values: sr.values || [], color: colorFor(sr.key) });
                return;
            }
            if (!other) other = { key: 'other', label: 'Other', values: (sr.values || []).map(() => 0), color: OTHER };
            (sr.values || []).forEach((v, j) => { other.values[j] = num(other.values[j]) + num(v); });
            if (sr.key !== 'other') folded.push(sr.label || sr.key);
        });
        if (other && other.values.some((v) => v)) {
            if (folded.length) other.label = 'Other (' + folded.slice(0, 4).join(', ') + (folded.length > 4 ? ', …' : '') + ')';
            out.push(other);
        }
        return out;
    }

    // ------------------------------------------------------------------
    // Scales
    // ------------------------------------------------------------------

    function niceStep(raw) {
        const pow = Math.pow(10, Math.floor(Math.log10(raw)));
        const f = raw / pow;
        const nice = f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10;
        return nice * pow;
    }

    // Ticks from 0 to a clean value at or above `max`. Byte axes step in the
    // binary unit the maximum reads in (0, 256 MiB, 512 MiB, ...). Counts
    // (requests, episodes, accounts) only come whole: their step is at
    // least 1, so a small axis reads 0, 1, 2 and never 0.3.
    function ticks(max, kind, count) {
        const n = count || 4;
        if (!(max > 0)) return [0, 1];
        let unit = 1;
        if (kind === 'bytes') {
            while (max / unit >= 1024) unit *= 1024;
        }
        let step = niceStep(max / unit / n) * unit;
        if (kind === 'count' || kind === 'int' || kind === 'bytes') step = Math.max(1, step);
        const out = [];
        for (let v = 0; v <= max + step * 0.0001; v += step) out.push(v);
        if (out[out.length - 1] < max) out.push(out[out.length - 1] + step);
        return out;
    }

    function axisFormat(kind) {
        if (kind === 'bytes') return fmtBytes;
        if (kind === 'pct') return (v) => intFmt.format(v) + '%';
        if (kind === 'ratio') return (v) => intFmt.format(v * 100) + '%';
        if (kind === 'ms') return fmtMs;
        if (kind === 'minutes') return fmtMinutes;
        return fmtNum;
    }

    // ------------------------------------------------------------------
    // Shared figure pieces
    // ------------------------------------------------------------------

    function emptyState(text) {
        return h('div', { class: 'bc-empty', text: text || 'Nothing recorded in this range.' });
    }

    function legend(items, opts) {
        const list = h('ul', { class: 'bc-legend', 'aria-label': 'Legend' });
        items.forEach((item) => {
            const swatch = h('span', { class: 'bc-legend__swatch bc-legend__swatch--' + (opts.mark || 'rect'), 'aria-hidden': 'true' });
            swatch.style.setProperty('--c', item.color);
            const content = [swatch, h('span', { class: 'bc-legend__label', text: item.label })];
            if (opts.onToggle) {
                const btn = h('button', {
                    type: 'button',
                    class: 'bc-legend__btn',
                    'aria-pressed': item.hidden ? 'false' : 'true',
                    title: item.hidden ? 'Show ' + item.label : 'Hide ' + item.label,
                    onclick: () => opts.onToggle(item.key),
                }, content);
                list.appendChild(h('li', { class: item.hidden ? 'bc-legend__item is-off' : 'bc-legend__item' }, btn));
            } else {
                list.appendChild(h('li', { class: 'bc-legend__item' }, content));
            }
        });
        return list;
    }

    function dataTable(headers, rows, caption) {
        const table = h('table', { class: 'table bc-datatable' });
        if (caption) table.appendChild(h('caption', { class: 'bc-sr', text: caption }));
        table.appendChild(h('thead', null, h('tr', null, headers.map((label, i) =>
            h('th', { scope: 'col', class: i ? 'is-num' : null, text: label })))));
        table.appendChild(h('tbody', null, rows.map((cells) =>
            h('tr', null, cells.map((cell, i) => h(i ? 'td' : 'th', { scope: i ? null : 'row', class: i ? 'is-num' : null, text: cell }))))));
        return table;
    }

    // A "Show as table" disclosure under a chart: the accessible twin of
    // every figure, and the home of every value a tooltip shows.
    function tableToggle(build) {
        const wrap = h('div', { class: 'bc-tableview' });
        const region = h('div', { class: 'bc-tableview__body', hidden: true });
        const btn = h('button', {
            type: 'button',
            class: 'btn btn--ghost btn--sm bc-tableview__btn',
            'aria-expanded': 'false',
            text: 'Show as table',
        });
        btn.addEventListener('click', () => {
            const open = region.hidden;
            region.hidden = !open;
            btn.setAttribute('aria-expanded', open ? 'true' : 'false');
            btn.textContent = open ? 'Hide table' : 'Show as table';
            if (open && !region.firstChild) region.appendChild(h('div', { class: 'bc-scroll' }, build()));
        });
        wrap.appendChild(btn);
        wrap.appendChild(region);
        return wrap;
    }

    // Re-render a figure when its box changes width (a tab that was hidden
    // when it was drawn, a rotated phone). Width only: height is ours.
    function onWidth(node, draw) {
        let last = -1;
        let frame = 0;
        const run = () => {
            frame = 0;
            const w = Math.floor(node.clientWidth);
            if (w > 0 && w !== last) {
                last = w;
                draw(w);
            }
        };
        if ('ResizeObserver' in window) {
            const ro = new ResizeObserver(() => {
                if (!frame) frame = requestAnimationFrame(run);
            });
            ro.observe(node);
        }
        run();
        if (last < 0) draw(640);
    }

    // ------------------------------------------------------------------
    // Time series: line, stacked area, columns
    // ------------------------------------------------------------------

    // opts: { t, series:[{key,label,values,color?}], granularity, type:
    // 'line'|'area'|'bar', stacked, kind (value format), colorFor(key),
    // maxSeries, height, label (accessible name), emptyText, totalLabel,
    // gaps }. With `gaps` (readings, not counts), a null value is "no
    // reading then": a line breaks there instead of dropping to 0, a 0 is a
    // reading like any other, and the axis starts at the first reading (a
    // server that started an hour ago draws its hour, not a day of nothing).
    function timeSeries(opts) {
        const fig = h('figure', { class: 'bc-chart' });
        let t = opts.t || [];
        const kind = opts.kind || 'count';
        const fmt = formatter(kind);
        const gaps = !!opts.gaps;
        const isGap = (v) => v === null || v === undefined;
        const value = (v) => (gaps && isGap(v) ? null : num(v));
        const fmtCell = (v) => (isGap(v) ? '–' : fmt(v));
        const colorFor = opts.colorFor || ((key) => colorScale('default')(key));
        let series = (opts.series || []).map((sr) => ({ key: sr.key, label: sr.label || sr.key, values: (sr.values || []).map(value), color: sr.color }));
        if (gaps) {
            const first = t.findIndex((_, i) => series.some((sr) => !isGap(sr.values[i])));
            if (first > 0) {
                t = t.slice(first);
                series = series.map((sr) => ({ ...sr, values: sr.values.slice(first) }));
            }
        }
        series = series.map((sr) => ({ ...sr, color: sr.color || colorFor(sr.key) }));
        if (series.some((sr) => !sr.color) || series.length > (opts.maxSeries || SLOTS)) {
            // Eight hues and a grey Other: never a ninth hue.
            series = foldSeries(series, (key) => series.find((x) => x.key === key)?.color, opts.maxSeries || SLOTS);
        }
        const hasData = t.length > 0 && series.some((sr) => sr.values.some((v) => (gaps ? !isGap(v) : v !== 0)));
        if (!hasData) {
            fig.appendChild(emptyState(opts.emptyText));
            return fig;
        }
        const hidden = new Set();
        const legendSlot = h('div', { class: 'bc-chart__legend' });
        const plot = h('div', { class: 'bc-plot' });
        const tip = h('div', { class: 'bc-tip', role: 'presentation', hidden: true });
        const live = h('div', { class: 'bc-sr', 'aria-live': 'polite' });
        plot.appendChild(tip);
        fig.appendChild(legendSlot);
        fig.appendChild(plot);
        fig.appendChild(live);
        const stacked = opts.type === 'area' || (opts.type === 'bar' && series.length > 1) || opts.stacked;
        const mark = opts.type === 'line' ? 'line' : 'rect';
        const span = t.length > 1 ? t[t.length - 1] - t[0] : 0;

        function drawLegend() {
            clear(legendSlot);
            if (series.length < 2) return;
            legendSlot.appendChild(legend(series.map((sr) => ({ key: sr.key, label: sr.label, color: sr.color, hidden: hidden.has(sr.key) })), {
                mark,
                onToggle: (key) => {
                    if (hidden.has(key)) hidden.delete(key);
                    else if (hidden.size < series.length - 1) hidden.add(key);
                    drawLegend();
                    redraw();
                },
            }));
        }

        let width = 640;
        let state = null;
        function redraw() {
            draw(width);
        }

        function draw(w) {
            width = w;
            const visible = series.filter((sr) => !hidden.has(sr.key));
            const n = t.length;
            const height = opts.height || 220;
            // Stack tops per point, for the axis maximum and the stacking.
            const tops = [];
            let max = 0;
            for (let i = 0; i < n; i += 1) {
                let acc = 0;
                const row = [];
                visible.forEach((sr) => {
                    const v = Math.max(0, sr.values[i] || 0);
                    const y0 = stacked ? acc : 0;
                    const y1 = stacked ? acc + v : v;
                    acc = stacked ? y1 : acc;
                    row.push([y0, y1]);
                    if (y1 > max) max = y1;
                });
                tops.push(row);
            }
            const yt = ticks(max, kind, height < 160 ? 2 : 4);
            const yMax = yt[yt.length - 1] || 1;
            const af = axisFormat(kind);
            const labelW = Math.max(...yt.map((v) => af(v).length)) * 6.6 + 10;
            const m = { l: Math.ceil(labelW), r: 12, t: 10, b: 26 };
            const pw = Math.max(40, w - m.l - m.r);
            const ph = height - m.t - m.b;
            const bar = opts.type === 'bar';
            const band = n > 0 ? pw / n : pw;
            const x = (i) => (bar ? m.l + band * (i + 0.5) : (n === 1 ? m.l + pw / 2 : m.l + (pw * i) / (n - 1)));
            const y = (v) => m.t + ph - (ph * v) / yMax;

            const root = s('svg', {
                class: 'bc-svg',
                width: w,
                height,
                viewBox: '0 0 ' + w + ' ' + height,
                role: 'img',
                'aria-label': (opts.label || 'Chart') + ', ' + fmtTime(t[0], opts.granularity) + ' to ' + fmtTime(t[n - 1], opts.granularity),
            });
            // Grid and y ticks.
            const grid = s('g', { class: 'bc-grid' });
            yt.forEach((v) => {
                const yy = Math.round(y(v)) + 0.5;
                grid.appendChild(s('line', { x1: m.l, x2: m.l + pw, y1: yy, y2: yy, class: v === 0 ? 'bc-baseline' : 'bc-gridline' }));
                grid.appendChild(s('text', { x: m.l - 6, y: yy + 4, class: 'bc-tick', 'text-anchor': 'end', text: af(v) }));
            });
            root.appendChild(grid);
            // X ticks: about one per 90px, snapped to points.
            const xCount = Math.max(2, Math.min(n, Math.floor(pw / 80)));
            const xg = s('g', { class: 'bc-xaxis' });
            const seen = new Set();
            for (let k = 0; k < xCount; k += 1) {
                const i = n === 1 ? 0 : Math.round((k * (n - 1)) / (xCount - 1));
                if (seen.has(i)) continue;
                seen.add(i);
                const anchor = k === 0 && !bar ? 'start' : (k === xCount - 1 && !bar ? 'end' : 'middle');
                xg.appendChild(s('text', { x: x(i), y: height - 8, class: 'bc-tick', 'text-anchor': anchor, text: fmtTick(t[i], opts.granularity, span) }));
            }
            root.appendChild(xg);

            const marks = s('g', { class: 'bc-marks' });
            if (bar) {
                const bw = Math.max(1, Math.min(24, band - 2));
                visible.forEach((sr, si) => {
                    for (let i = 0; i < n; i += 1) {
                        const [y0, y1] = tops[i][si];
                        if (y1 <= y0) continue;
                        const top = y(y1);
                        // 2px surface gap between stacked segments.
                        const bottom = y(y0) - (si > 0 && stacked ? 2 : 0);
                        const hgt = Math.max(0, bottom - top);
                        if (hgt <= 0) continue;
                        const isTop = si === visible.length - 1 || !stacked;
                        marks.appendChild(s('path', {
                            d: roundedTop(x(i) - bw / 2, top, bw, hgt, isTop ? Math.min(4, bw / 2, hgt) : 0),
                            fill: sr.color,
                        }));
                    }
                });
            } else if (gaps && !stacked) {
                // Runs of readings, each its own line; a lone reading is a dot.
                visible.forEach((sr, si) => {
                    const runs = [];
                    let run = [];
                    for (let i = 0; i < n; i += 1) {
                        if (isGap(sr.values[i])) {
                            if (run.length) runs.push(run);
                            run = [];
                        } else run.push(i);
                    }
                    if (run.length) runs.push(run);
                    runs.forEach((r) => {
                        const d = r.map((i, k) => (k ? 'L' : 'M') + x(i).toFixed(1) + ' ' + y(tops[i][si][1]).toFixed(1)).join('');
                        if (r.length === 1) {
                            marks.appendChild(s('circle', { cx: x(r[0]), cy: y(tops[r[0]][si][1]), r: 2.5, fill: sr.color, class: 'bc-dot' }));
                            return;
                        }
                        if (visible.length === 1) {
                            marks.appendChild(s('path', { d: d + 'L' + x(r[r.length - 1]).toFixed(1) + ' ' + y(0) + 'L' + x(r[0]).toFixed(1) + ' ' + y(0) + 'Z', fill: sr.color, class: 'bc-wash' }));
                        }
                        marks.appendChild(s('path', { d, stroke: sr.color, class: 'bc-line' }));
                    });
                });
            } else {
                visible.forEach((sr, si) => {
                    let top = '';
                    let bottom = '';
                    for (let i = 0; i < n; i += 1) {
                        const [y0, y1] = tops[i][si];
                        top += (i ? 'L' : 'M') + x(i).toFixed(1) + ' ' + y(y1).toFixed(1);
                        bottom = 'L' + x(i).toFixed(1) + ' ' + y(y0).toFixed(1) + bottom;
                    }
                    if (stacked) {
                        marks.appendChild(s('path', { d: top + bottom + 'Z', fill: sr.color, class: 'bc-area' }));
                    } else if (visible.length === 1) {
                        marks.appendChild(s('path', { d: top + 'L' + x(n - 1).toFixed(1) + ' ' + y(0) + 'L' + x(0).toFixed(1) + ' ' + y(0) + 'Z', fill: sr.color, class: 'bc-wash' }));
                    }
                    marks.appendChild(s('path', { d: top, stroke: sr.color, class: 'bc-line' }));
                });
                if (n === 1) {
                    visible.forEach((sr, si) => marks.appendChild(s('circle', { cx: x(0), cy: y(tops[0][si][1]), r: 4, fill: sr.color, class: 'bc-dot' })));
                }
            }
            root.appendChild(marks);

            // The hover layer: a crosshair that snaps to the nearest point.
            const cross = s('g', { class: 'bc-cross', visibility: 'hidden' });
            const crossLine = s('line', { y1: m.t, y2: m.t + ph, class: 'bc-crossline' });
            cross.appendChild(crossLine);
            const dots = visible.map((sr) => {
                const dot = s('circle', { r: 4, fill: sr.color, class: 'bc-dot' });
                cross.appendChild(dot);
                return dot;
            });
            root.appendChild(cross);
            const hit = s('rect', { x: m.l, y: m.t, width: pw, height: ph, class: 'bc-hit' });
            root.appendChild(hit);

            const old = plot.querySelector('svg');
            if (old) plot.replaceChild(root, old);
            else plot.insertBefore(root, tip);
            state = { visible, tops, x, y, n, m, pw, cross, crossLine, dots, root, bar };
            if (focusIndex !== null) show(Math.min(focusIndex, n - 1));
        }

        function indexAt(clientX) {
            const rect = state.root.getBoundingClientRect();
            const px = clientX - rect.left;
            const { n, m, pw, bar } = state;
            if (n <= 1) return 0;
            const f = bar ? (px - m.l) / (pw / n) - 0.5 : ((px - m.l) / pw) * (n - 1);
            return Math.max(0, Math.min(n - 1, Math.round(f)));
        }

        let focusIndex = null;
        function show(i) {
            const st = state;
            if (!st) return;
            const cx = st.x(i);
            st.cross.setAttribute('visibility', 'visible');
            st.crossLine.setAttribute('x1', cx);
            st.crossLine.setAttribute('x2', cx);
            st.dots.forEach((dot, si) => {
                dot.setAttribute('cx', cx);
                dot.setAttribute('cy', st.y(st.tops[i][si][1]));
                dot.setAttribute('visibility', st.bar || isGap(st.visible[si].values[i]) ? 'hidden' : 'visible');
            });
            clear(tip);
            tip.appendChild(h('div', { class: 'bc-tip__head', text: fmtTime(t[i], opts.granularity, true) }));
            const rows = st.visible.slice().reverse();
            let total = 0;
            const words = [];
            rows.forEach((sr) => {
                const raw = sr.values[i];
                const v = isGap(raw) ? 0 : raw;
                total += v;
                const key = h('span', { class: 'bc-tip__key', 'aria-hidden': 'true' });
                key.style.setProperty('--c', sr.color);
                const shown = isGap(raw) ? 'no reading' : fmt(v);
                tip.appendChild(h('div', { class: 'bc-tip__row' }, [key, h('strong', { text: shown }), h('span', { text: sr.label })]));
                words.push(sr.label + ' ' + shown);
            });
            if (stacked && rows.length > 1) {
                tip.appendChild(h('div', { class: 'bc-tip__row bc-tip__total' }, [h('span', { class: 'bc-tip__key bc-tip__key--none' }), h('strong', { text: fmt(total) }), h('span', { text: opts.totalLabel || 'Total' })]));
            }
            tip.hidden = false;
            const plotW = plot.clientWidth;
            const tw = tip.offsetWidth;
            let left = cx + 12;
            if (left + tw > plotW) left = cx - 12 - tw;
            tip.style.left = Math.max(0, left) + 'px';
            tip.style.top = (st.m.t) + 'px';
            live.textContent = fmtTime(t[i], opts.granularity, true) + ': ' + words.join(', ');
        }

        function hide() {
            if (state) state.cross.setAttribute('visibility', 'hidden');
            tip.hidden = true;
        }

        plot.addEventListener('pointermove', (e) => {
            if (!state) return;
            focusIndex = null;
            show(indexAt(e.clientX));
        });
        plot.addEventListener('pointerleave', () => {
            if (focusIndex === null) hide();
        });
        // Keyboard: the plot takes focus; arrows walk the points.
        plot.tabIndex = 0;
        plot.setAttribute('role', 'group');
        plot.setAttribute('aria-roledescription', 'chart');
        plot.setAttribute('aria-label', (opts.label || 'Chart') + '. Use the left and right arrow keys to read each point.');
        plot.addEventListener('focus', () => {
            focusIndex = t.length - 1;
            show(focusIndex);
        });
        plot.addEventListener('blur', () => {
            focusIndex = null;
            hide();
        });
        plot.addEventListener('keydown', (e) => {
            if (focusIndex === null) return;
            let next = focusIndex;
            if (e.key === 'ArrowLeft') next -= 1;
            else if (e.key === 'ArrowRight') next += 1;
            else if (e.key === 'Home') next = 0;
            else if (e.key === 'End') next = t.length - 1;
            else if (e.key === 'PageUp') next -= 7;
            else if (e.key === 'PageDown') next += 7;
            else if (e.key === 'Escape') { plot.blur(); return; } else return;
            e.preventDefault();
            focusIndex = Math.max(0, Math.min(t.length - 1, next));
            show(focusIndex);
        });

        drawLegend();
        fig.appendChild(tableToggle(() => {
            const headers = [opts.granularity === 'hour' ? 'Time' : 'Day (UTC)'].concat(series.map((sr) => sr.label));
            if (stacked && series.length > 1) headers.push(opts.totalLabel || 'Total');
            const rows = t.map((ts, i) => {
                const cells = [fmtTime(ts, opts.granularity, true)].concat(series.map((sr) => fmtCell(sr.values[i])));
                if (stacked && series.length > 1) cells.push(fmt(series.reduce((a, sr) => a + (sr.values[i] || 0), 0)));
                return cells;
            }).reverse();
            return dataTable(headers, rows, opts.label);
        }));
        // Drawn once the figure is in the page and has a width.
        requestAnimationFrame(() => onWidth(plot, draw));
        return fig;
    }

    // A column with a rounded data end and a square baseline.
    function roundedTop(x, y, w, hgt, r) {
        if (!r) return 'M' + x + ' ' + (y + hgt) + 'V' + y + 'H' + (x + w) + 'V' + (y + hgt) + 'Z';
        return 'M' + x + ' ' + (y + hgt) +
            'V' + (y + r) +
            'Q' + x + ' ' + y + ' ' + (x + r) + ' ' + y +
            'H' + (x + w - r) +
            'Q' + (x + w) + ' ' + y + ' ' + (x + w) + ' ' + (y + r) +
            'V' + (y + hgt) + 'Z';
    }

    // ------------------------------------------------------------------
    // Horizontal bar list
    // ------------------------------------------------------------------

    // rows: [{key,label,value,sub?,color?,title?,badge?}]; opts: {kind,
    // color, onSelect(row), selectedKey, limit, emptyText, label}
    function barList(rows, opts) {
        const o = opts || {};
        const fmt = formatter(o.kind);
        const list = (rows || []).filter((r) => r);
        if (!list.length || list.every((r) => !num(r.value))) return emptyState(o.emptyText);
        const shown = o.limit ? list.slice(0, o.limit) : list;
        const max = Math.max(...shown.map((r) => num(r.value)), 1);
        const ul = h('ul', { class: 'bc-bars', 'aria-label': o.label || null });
        shown.forEach((row) => {
            const fill = h('span', { class: 'bc-bars__fill' });
            fill.style.width = Math.max(num(row.value) > 0 ? 0.5 : 0, (100 * num(row.value)) / max) + '%';
            fill.style.setProperty('--c', row.color || o.color || 'var(--an-s1)');
            const value = h('span', { class: 'bc-bars__value', text: row.valueText || fmt(row.value) });
            const label = h('span', { class: 'bc-bars__label' }, [
                row.badge ? h('span', { class: 'badge badge--muted bc-bars__badge', text: row.badge }) : null,
                h('span', { class: 'bc-bars__name', text: row.label, title: row.title || row.label }),
            ]);
            const parts = [label, value, h('span', { class: 'bc-bars__track', 'aria-hidden': 'true' }, fill)];
            if (row.sub) parts.push(h('span', { class: 'bc-bars__sub', text: row.sub }));
            let item;
            if (o.onSelect) {
                item = h('button', {
                    type: 'button',
                    class: 'bc-bars__row bc-bars__row--btn' + (o.selectedKey && o.selectedKey === row.key ? ' is-selected' : ''),
                    'aria-pressed': o.selectedKey === row.key ? 'true' : 'false',
                    onclick: () => o.onSelect(row),
                }, parts);
            } else {
                item = h('div', { class: 'bc-bars__row' }, parts);
            }
            ul.appendChild(h('li', null, item));
        });
        const wrap = h('div', { class: 'bc-barlist' }, ul);
        if (o.limit && list.length > o.limit) wrap.appendChild(h('p', { class: 'bc-note', text: (list.length - o.limit) + ' more not shown.' }));
        return wrap;
    }

    // ------------------------------------------------------------------
    // Sparkline, KPI tiles, segmented bar
    // ------------------------------------------------------------------

    function sparkline(values, opts) {
        const o = opts || {};
        const vals = (values || []).map(num);
        const w = o.width || 96;
        const hh = o.height || 28;
        if (vals.length < 2) return null;
        const max = Math.max(...vals, 1);
        const pts = vals.map((v, i) => [(i * (w - 6)) / (vals.length - 1) + 3, hh - 3 - ((hh - 6) * v) / max]);
        const d = pts.map((p, i) => (i ? 'L' : 'M') + p[0].toFixed(1) + ' ' + p[1].toFixed(1)).join('');
        const last = pts[pts.length - 1];
        return s('svg', { class: 'bc-spark', width: w, height: hh, viewBox: '0 0 ' + w + ' ' + hh, 'aria-hidden': 'true' }, [
            s('path', { d, class: 'bc-spark__line' }),
            s('circle', { cx: last[0], cy: last[1], r: 3, class: 'bc-spark__dot' }),
        ]);
    }

    // tiles: [{label, value, sub?, delta?:{ratio, upIsGood}, spark?:[...],
    // tone?:'warn'|'bad'}]
    function kpis(tiles) {
        const grid = h('div', { class: 'bc-kpis' });
        tiles.filter(Boolean).forEach((tile) => {
            const node = h('div', { class: 'bc-kpi' + (tile.tone ? ' bc-kpi--' + tile.tone : '') });
            node.appendChild(h('div', { class: 'bc-kpi__label', text: tile.label }));
            const valueRow = h('div', { class: 'bc-kpi__row' }, h('div', { class: 'bc-kpi__value', text: tile.value }));
            if (tile.spark) {
                const sp = sparkline(tile.spark);
                if (sp) valueRow.appendChild(sp);
            }
            node.appendChild(valueRow);
            if (tile.delta) node.appendChild(trend(tile.delta.cur, tile.delta.prev, tile.delta.upIsGood !== false, tile.delta.period));
            if (tile.sub) node.appendChild(h('div', { class: 'bc-kpi__sub', text: tile.sub }));
            if (tile.title) node.title = tile.title;
            grid.appendChild(node);
        });
        return grid;
    }

    // An arrow and a signed change against the previous period: the arrow
    // and the sign carry the direction, colour only seconds them.
    function trend(cur, prev, upIsGood, period) {
        const c = num(cur);
        const p = num(prev);
        const versus = period || 'the previous period';
        if (!p && !c) return h('span', { class: 'bc-trend bc-trend--flat', text: '–', title: 'No change vs ' + versus, 'aria-label': 'no change vs ' + versus });
        if (!p) return h('span', { class: 'bc-trend bc-trend--new', text: 'new', title: 'None in ' + versus, 'aria-label': 'new: none in ' + versus });
        const ratio = (c - p) / p;
        const pct = Math.round(Math.abs(ratio) * 100);
        if (pct === 0) return h('span', { class: 'bc-trend bc-trend--flat', text: '– 0%', 'aria-label': 'no change vs ' + versus });
        const up = ratio > 0;
        const good = up === upIsGood;
        return h('span', {
            class: 'bc-trend ' + (good ? 'bc-trend--good' : 'bc-trend--bad'),
            text: (up ? '▲ +' : '▼ −') + (pct > 999 ? '999+' : pct) + '%',
            title: (up ? 'Up ' : 'Down ') + pct + '% vs ' + versus + ' (' + fmtNum(p) + ')',
            'aria-label': (up ? 'up ' : 'down ') + pct + '% vs ' + versus,
        });
    }

    // One whole split into parts. parts: [{key,label,value,color}];
    // opts: {kind, tall, label}
    function segmented(parts, opts) {
        const o = opts || {};
        const fmt = formatter(o.kind);
        const list = (parts || []).filter((p) => num(p.value) > 0);
        const total = list.reduce((a, p) => a + num(p.value), 0);
        if (!total) return emptyState(o.emptyText);
        const desc = list.map((p) => p.label + ' ' + fmt(p.value) + ' (' + fmtRatio(num(p.value) / total) + ')').join(', ');
        const bar = h('div', { class: 'bc-seg' + (o.tall ? ' bc-seg--tall' : ''), role: 'img', 'aria-label': (o.label ? o.label + ': ' : '') + desc });
        list.forEach((p) => {
            const share = num(p.value) / total;
            const block = h('div', { class: 'bc-seg__part', title: p.label + ': ' + fmt(p.value) + ' (' + fmtRatio(share) + ')' });
            block.style.flexGrow = String(share);
            block.style.setProperty('--c', p.color || OTHER);
            if (o.tall) block.appendChild(h('span', { class: 'bc-seg__text', text: fmtRatio(share) }));
            bar.appendChild(block);
        });
        const wrap = h('div', { class: 'bc-segwrap' }, bar);
        if (o.legend !== false) wrap.appendChild(legend(list.map((p) => ({ key: p.key, label: p.label, color: p.color || OTHER })), { mark: 'rect' }));
        if (o.tall) {
            // Measure first: a share that does not fit its block is left to
            // the hover title and the legend, never clipped.
            requestAnimationFrame(() => {
                bar.querySelectorAll('.bc-seg__text').forEach((text) => {
                    if (text.scrollWidth > text.parentNode.clientWidth - 8) text.remove();
                });
            });
        }
        return wrap;
    }

    // ------------------------------------------------------------------
    // Funnel: per volume, downloads -> started -> finished
    // ------------------------------------------------------------------

    function funnel(volumes, opts) {
        const o = opts || {};
        const list = volumes || [];
        if (!list.length) return emptyState(o.emptyText || 'No volumes in this series.');
        const stages = [
            { key: 'downloads', label: 'Downloaded', color: 'var(--an-seq-1)' },
            { key: 'started', label: 'Started', color: 'var(--an-seq-2)' },
            { key: 'finished', label: 'Finished', color: 'var(--an-seq-3)' },
        ];
        const max = Math.max(1, ...list.map((v) => Math.max(num(v.downloads), num(v.started), num(v.finished))));
        const wrap = h('div', { class: 'bc-funnel' });
        wrap.appendChild(legend(stages, { mark: 'rect' }));
        const ol = h('ol', { class: 'bc-funnel__list' });
        list.forEach((vol) => {
            const rows = stages.map((st) => {
                const fill = h('span', { class: 'bc-bars__fill' });
                fill.style.width = (100 * num(vol[st.key])) / max + '%';
                fill.style.setProperty('--c', st.color);
                return h('div', { class: 'bc-funnel__stage' }, [
                    h('span', { class: 'bc-bars__track', 'aria-hidden': 'true' }, fill),
                    h('span', { class: 'bc-funnel__num', text: fmtInt(vol[st.key]) }),
                ]);
            });
            ol.appendChild(h('li', {
                class: 'bc-funnel__vol',
                'aria-label': (vol.title || vol.volume_key) + ': ' + stages.map((st) => st.label.toLowerCase() + ' ' + fmtInt(vol[st.key])).join(', '),
            }, [h('span', { class: 'bc-funnel__title', text: vol.title || vol.volume_key, title: vol.volume_key || '' }), h('div', { class: 'bc-funnel__stages', 'aria-hidden': 'true' }, rows)]));
        });
        wrap.appendChild(ol);
        wrap.appendChild(tableToggle(() => dataTable(['Volume'].concat(stages.map((st) => st.label)),
            list.map((v) => [v.title || v.volume_key].concat(stages.map((st) => fmtInt(v[st.key])))), 'Completion funnel')));
        return wrap;
    }

    // ------------------------------------------------------------------
    // Sortable table
    // ------------------------------------------------------------------

    // columns: [{key, label, kind?, num?, render(row)->string|Node,
    // sortValue(row), sortable?, className?, title?}]
    // opts: {rows, sortKey, sortDir, onSort(key, dir) (server-side sort;
    // otherwise sorted here), onRowClick(row), rowKey(row), selectedKey,
    // emptyText, caption, limit}
    function table(columns, opts) {
        const o = opts || {};
        let sortKey = o.sortKey || null;
        let sortDir = o.sortDir || 'desc';
        const wrap = h('div', { class: 'table-container bc-tablewrap' });
        const tbl = h('table', { class: 'table bc-table' });
        if (o.caption) tbl.appendChild(h('caption', { class: 'bc-sr', text: o.caption }));
        const theadRow = h('tr');
        tbl.appendChild(h('thead', null, theadRow));
        const tbody = h('tbody');
        tbl.appendChild(tbody);
        wrap.appendChild(tbl);

        function value(col, row) {
            if (col.sortValue) return col.sortValue(row);
            return row[col.key];
        }

        function cell(col, row) {
            if (col.render) return col.render(row);
            const v = row[col.key];
            if (col.kind) return formatter(col.kind)(v);
            return v === undefined || v === null || v === '' ? '-' : String(v);
        }

        function drawHead() {
            clear(theadRow);
            columns.forEach((col) => {
                const numeric = col.num || (col.kind && col.kind !== 'text');
                const th = h('th', { scope: 'col', class: (numeric ? 'is-num ' : '') + (col.className || '') });
                if (col.sortable === false) {
                    th.textContent = col.label;
                } else {
                    const active = sortKey === col.key;
                    th.setAttribute('aria-sort', active ? (sortDir === 'asc' ? 'ascending' : 'descending') : 'none');
                    th.appendChild(h('button', {
                        type: 'button',
                        class: 'bc-sort' + (active ? ' is-active' : ''),
                        title: col.title || null,
                        onclick: () => {
                            // A server-sorted table sorts one way (largest
                            // first): the server is asked, not the rows.
                            if (o.serverSort) {
                                sortKey = col.key;
                                sortDir = 'desc';
                            } else if (sortKey === col.key) sortDir = sortDir === 'desc' ? 'asc' : 'desc';
                            else {
                                sortKey = col.key;
                                sortDir = numeric ? 'desc' : 'asc';
                            }
                            if (o.onSort) o.onSort(sortKey, sortDir);
                            drawHead();
                            drawBody();
                        },
                    }, [col.label, h('span', { class: 'bc-sort__arrow', 'aria-hidden': 'true', text: active ? (sortDir === 'asc' ? '▲' : '▼') : '' })]));
                }
                theadRow.appendChild(th);
            });
        }

        function drawBody() {
            clear(tbody);
            let rows = (o.rows || []).slice();
            if (!rows.length) {
                tbody.appendChild(h('tr', null, h('td', { colspan: columns.length, class: 'bc-table__empty', text: o.emptyText || 'Nothing recorded in this range.' })));
                return;
            }
            const col = columns.find((c) => c.key === sortKey);
            if (col && !o.serverSort) {
                const dir = sortDir === 'asc' ? 1 : -1;
                rows.sort((a, b) => {
                    const va = value(col, a);
                    const vb = value(col, b);
                    if (typeof va === 'number' || typeof vb === 'number') return (num(va) - num(vb)) * dir;
                    return String(va || '').localeCompare(String(vb || '')) * dir;
                });
            }
            if (o.limit) rows = rows.slice(0, o.limit);
            rows.forEach((row) => {
                const key = o.rowKey ? o.rowKey(row) : null;
                const tr = h('tr', { class: o.selectedKey && key === o.selectedKey ? 'is-selected' : null });
                columns.forEach((c) => {
                    const numeric = c.num || (c.kind && c.kind !== 'text');
                    const content = cell(c, row);
                    tr.appendChild(h('td', { class: (numeric ? 'is-num ' : '') + (c.className || '') }, content));
                });
                if (o.onRowClick) {
                    tr.classList.add('is-clickable');
                    tr.tabIndex = 0;
                    if (o.rowLabel) tr.setAttribute('aria-label', o.rowLabel(row));
                    tr.addEventListener('click', (e) => {
                        if (e.target.closest('a, button, summary, details')) return;
                        o.onRowClick(row);
                    });
                    tr.addEventListener('keydown', (e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                            e.preventDefault();
                            o.onRowClick(row);
                        }
                    });
                }
                tbody.appendChild(tr);
            });
        }

        drawHead();
        drawBody();
        return wrap;
    }

    window.BunkoCharts = {
        h,
        s,
        clear,
        nextId,
        num,
        fmtBytes,
        fmtNum,
        fmtInt,
        fmtRatio,
        fmtPct,
        fmtMs,
        fmtMinutes,
        fmtDuration,
        fmtTime,
        fmtDateTime,
        fmtDate,
        fmtAgo,
        formatter,
        colorScale,
        slotColor,
        OTHER,
        foldSeries,
        ticks,
        emptyState,
        legend,
        timeSeries,
        barList,
        sparkline,
        kpis,
        trend,
        segmented,
        funnel,
        table,
        tableToggle,
        dataTable,
    };
})();
