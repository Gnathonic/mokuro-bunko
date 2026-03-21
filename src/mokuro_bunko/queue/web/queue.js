(function () {
  "use strict";

  const DEFAULT_POLL_INTERVAL = 3000;

  const $backend = document.getElementById("backend");
  const $workerSummary = document.getElementById("worker-summary");
  const $currentJob = document.getElementById("current-job");
  const $pendingOcrCount = document.getElementById("pending-ocr-count");
  const $pendingOcrList = document.getElementById("pending-ocr-list");
  const $pendingThumbCount = document.getElementById("pending-thumb-count");
  const $pendingThumbText = document.getElementById("pending-thumb-text");
  const $pendingSearch = document.getElementById("pending-search");
  const $ocrPauseBtn = document.getElementById("ocr-pause-btn");
  const $ocrResumeBtn = document.getElementById("ocr-resume-btn");
  const $ocrControlStatus = document.getElementById("ocr-control-status");
  const $refreshNow = document.getElementById("refresh-now");
  const $autoRefresh = document.getElementById("auto-refresh");
  const $refreshInterval = document.getElementById("refresh-interval");
  const $lastUpdated = document.getElementById("last-updated");
  const $metricWorker = document.getElementById("metric-worker");
  const $metricActive = document.getElementById("metric-active");
  const $metricPendingOcr = document.getElementById("metric-pending-ocr");
  const $metricPendingThumb = document.getElementById("metric-pending-thumb");
  const $historyStatus = document.getElementById("history-status");
  const $historySeries = document.getElementById("history-series");
  const $historySince = document.getElementById("history-since");
  const $historyLimit = document.getElementById("history-limit");
  const $historyClear = document.getElementById("history-clear");
  const $historyMeta = document.getElementById("history-meta");
  const $historyList = document.getElementById("history-list");
  const $queueToast = document.getElementById("queue-toast");

  let queueConfig = { show_in_nav: false, public_access: true };
  let pendingItems = [];
  let pollTimerId = null;
  let historyTimerId = null;
  let isControlInFlight = false;
  let lastStatusUpdateMs = 0;
  let staleTimerId = null;
  var PREFERENCES_KEY = "mokuro.queue.preferences.v1";

  function readPreferences() {
    try {
      var raw = localStorage.getItem(PREFERENCES_KEY);
      if (!raw) return {};
      var parsed = JSON.parse(raw);
      if (!parsed || typeof parsed !== "object") return {};
      return parsed;
    } catch (_err) {
      return {};
    }
  }

  function writePreferences(next) {
    try {
      localStorage.setItem(PREFERENCES_KEY, JSON.stringify(next));
    } catch (_err) {
      // ignore quota/storage errors
    }
  }

  function saveUiPreferences() {
    writePreferences({
      autoRefresh: !!($autoRefresh && $autoRefresh.checked),
      refreshInterval: ($refreshInterval && $refreshInterval.value) || String(DEFAULT_POLL_INTERVAL),
      historyStatus: ($historyStatus && $historyStatus.value) || "",
      historySeries: ($historySeries && $historySeries.value) || "",
      historySince: ($historySince && $historySince.value) || "",
      historyLimit: ($historyLimit && $historyLimit.value) || "25",
    });
  }

  function loadUiPreferences() {
    var prefs = readPreferences();
    if ($autoRefresh && typeof prefs.autoRefresh === "boolean") {
      $autoRefresh.checked = prefs.autoRefresh;
    }
    if ($refreshInterval && typeof prefs.refreshInterval === "string" && prefs.refreshInterval) {
      $refreshInterval.value = prefs.refreshInterval;
    }
    if ($historyStatus && typeof prefs.historyStatus === "string") {
      $historyStatus.value = prefs.historyStatus;
    }
    if ($historySeries && typeof prefs.historySeries === "string") {
      $historySeries.value = prefs.historySeries;
    }
    if ($historySince && typeof prefs.historySince === "string") {
      $historySince.value = prefs.historySince;
    }
    if ($historyLimit && typeof prefs.historyLimit === "string" && prefs.historyLimit) {
      $historyLimit.value = prefs.historyLimit;
    }
  }

  function getSessionAuth() {
    return sessionStorage.getItem("mokuro_auth");
  }

  function logout() {
    sessionStorage.removeItem("mokuro_auth");
    sessionStorage.removeItem("mokuro_user");
    window.location.href = "/";
  }

  async function updateNav() {
    if (window.renderMokuroHeaderNav) {
      await window.renderMokuroHeaderNav("queue");
    }
  }

  function getStatusHeaders() {
    const headers = {};
    if (!queueConfig.public_access) {
      const auth = getSessionAuth();
      if (auth) {
        headers.Authorization = "Basic " + auth;
      }
    }
    return headers;
  }

  function getAdminHeaders() {
    const headers = { "Content-Type": "application/json" };
    const auth = getSessionAuth();
    if (auth) {
      headers.Authorization = "Basic " + auth;
    }
    return headers;
  }

  function formatEta(seconds) {
    if (seconds == null) return "";
    if (seconds < 60) return seconds + "s";
    var m = Math.floor(seconds / 60);
    var s = seconds % 60;
    if (m < 60) return m + "m " + s + "s";
    var h = Math.floor(m / 60);
    m = m % 60;
    return h + "h " + m + "m";
  }

  function formatRelativeTime(unixSeconds) {
    if (!unixSeconds) return "-";
    var delta = Math.max(0, Math.floor(Date.now() / 1000 - unixSeconds));
    if (delta < 60) return delta + "s ago";
    if (delta < 3600) return Math.floor(delta / 60) + "m ago";
    if (delta < 86400) return Math.floor(delta / 3600) + "h ago";
    return Math.floor(delta / 86400) + "d ago";
  }

  function showToast(message, variant) {
    if (!$queueToast) return;
    $queueToast.textContent = message;
    $queueToast.className = "queue-toast queue-toast--show" + (variant ? " queue-toast--" + variant : "");
    window.clearTimeout(showToast._timer);
    showToast._timer = window.setTimeout(function () {
      $queueToast.className = "queue-toast";
    }, 2400);
  }

  function renderCurrent(current) {
    if (!current) {
      $currentJob.className = "current-job current-job--empty";
      $currentJob.innerHTML =
        '<p class="current-job__idle">Idle &mdash; no active OCR job</p>';
      return;
    }

    var pct = current.percent || 0;
    var pages =
      current.done_pages != null
        ? current.done_pages + (current.total_pages ? " / " + current.total_pages : "")
        : "";
    var eta = formatEta(current.eta_seconds);
    var statusClass =
      current.status === "error"
        ? "current-job__status--error"
        : current.status === "finalizing"
          ? "current-job__status--finalizing"
          : "";

    $currentJob.className = "current-job current-job--active";
    $currentJob.innerHTML =
      '<div class="current-job__header">' +
      '<span class="current-job__series">' + escapeHtml(current.series || "") + "</span>" +
      '<span class="current-job__volume">' + escapeHtml(current.volume || "") + "</span>" +
      "</div>" +
      '<div class="progress-bar">' +
      '<div class="progress-bar__fill" style="width:' + pct + '%"></div>' +
      "</div>" +
      '<div class="current-job__meta">' +
      '<span class="current-job__status ' + statusClass + '">' + escapeHtml(current.status || "running") + "</span>" +
      '<span class="current-job__pages">' + pages + " pages</span>" +
      '<span class="current-job__pct">' + pct + "%</span>" +
      (current.started_at ? '<span class="current-job__stamp">Started ' + escapeHtml(formatRelativeTime(current.started_at)) + "</span>" : "") +
      (eta ? '<span class="current-job__eta">ETA ' + eta + "</span>" : "") +
      "</div>";

    if (current.error) {
      $currentJob.innerHTML +=
        '<div class="current-job__error">' + escapeHtml(String(current.error)) + "</div>";
    }
  }

  function renderPendingOcr(list) {
    pendingItems = Array.isArray(list) ? list.slice() : [];
    $pendingOcrCount.textContent = pendingItems.length;
    applyPendingFilter();
  }

  function applyPendingFilter() {
    var search = (($pendingSearch && $pendingSearch.value) || "").trim().toLowerCase();
    var list = pendingItems;
    if (search) {
      list = pendingItems.filter(function (item) {
        var hay = ((item.series || "") + " " + (item.volume || "")).toLowerCase();
        return hay.indexOf(search) !== -1;
      });
    }

    if (!list.length) {
      $pendingOcrList.innerHTML =
        '<p class="pending-list__empty">No matching pending volumes</p>';
      return;
    }

    var html = '<ul class="pending-list__items">';
    for (var i = 0; i < list.length; i++) {
      html +=
        '<li class="pending-list__item">' +
        '<span class="pending-list__series">' + escapeHtml(list[i].series) + "</span>" +
        '<span class="pending-list__volume">' + escapeHtml(list[i].volume) + "</span>" +
        "</li>";
    }
    html += "</ul>";
    $pendingOcrList.innerHTML = html;
  }

  function renderPendingThumbs(count) {
    $pendingThumbCount.textContent = count;
    $pendingThumbText.textContent =
      count === 0
        ? "No volumes waiting for thumbnails"
        : count + " volume" + (count !== 1 ? "s" : "") + " waiting for thumbnail generation";
  }

  function renderWorkerControls(worker) {
    if (!worker || !worker.available) {
      $ocrPauseBtn.disabled = true;
      $ocrResumeBtn.disabled = true;
      $ocrControlStatus.textContent = "Worker unavailable";
      $workerSummary.textContent = "Worker unavailable";
      if ($metricWorker) {
        $metricWorker.textContent = "Unavailable";
      }
      document.body.classList.remove("queue-worker-running", "queue-worker-paused");
      return;
    }

    var paused = !!worker.paused;
    $ocrPauseBtn.disabled = paused || isControlInFlight;
    $ocrResumeBtn.disabled = !paused || isControlInFlight;
    $ocrControlStatus.textContent = paused ? "Worker paused" : "Worker running";
    $workerSummary.textContent = paused ? "Worker paused" : "Worker running";
    if ($metricWorker) {
      $metricWorker.textContent = paused ? "Paused" : "Running";
    }
    document.body.classList.toggle("queue-worker-paused", paused);
    document.body.classList.toggle("queue-worker-running", !paused);
  }

  function applyBackendState(backend) {
    if (!$backend) {
      return;
    }
    $backend.classList.remove("badge--backend-gpu", "badge--backend-cpu", "badge--backend-skip", "badge--backend-auto");
    var lower = String(backend || "unknown").toLowerCase();
    if (lower === "cuda" || lower === "rocm") {
      $backend.classList.add("badge--backend-gpu");
      return;
    }
    if (lower === "cpu") {
      $backend.classList.add("badge--backend-cpu");
      return;
    }
    if (lower === "skip") {
      $backend.classList.add("badge--backend-skip");
      return;
    }
    $backend.classList.add("badge--backend-auto");
  }

  function renderHistory(events) {
    if (!events || !events.length) {
      $historyList.innerHTML = '<p class="pending-list__empty">No OCR history yet</p>';
      return;
    }

    var html = '<ul class="history-list__items">';
    for (var i = 0; i < events.length; i++) {
      var item = events[i];
      var ts = item.timestamp ? new Date(item.timestamp * 1000).toLocaleString() : "-";
      var rel = item.timestamp ? formatRelativeTime(item.timestamp) : "-";
      var status = (item.status || "unknown").toLowerCase();
      var errorHtml = "";
      if (item.error) {
        errorHtml = '<div class="history-list__error">' + escapeHtml(String(item.error)) + "</div>";
      }
      html +=
        '<li class="history-list__item">' +
        '<div class="history-list__top">' +
        '<span class="history-list__status history-list__status--' + escapeHtml(status) + '">' + escapeHtml(status) + "</span>" +
        '<span class="history-list__time">' + escapeHtml(ts) + "</span>" +
        "</div>" +
        '<div class="history-list__title">' +
        '<span class="history-list__series">' + escapeHtml(item.series || "-") + "</span>" +
        '<span class="history-list__volume">' + escapeHtml(item.volume || "-") + "</span>" +
        "</div>" +
        '<div class="history-list__meta">' +
        '<span>' + escapeHtml(rel) + "</span>" +
        (item.percent != null ? '<span>' + escapeHtml(String(item.percent)) + '% complete</span>' : "") +
        (item.relative_cbz ? '<span>' + escapeHtml(String(item.relative_cbz)) + "</span>" : "") +
        "</div>" +
        errorHtml +
        "</li>";
    }
    html += "</ul>";
    $historyList.innerHTML = html;

    if ($historyMeta) {
      $historyMeta.textContent = "Showing " + events.length + " recent events";
    }
  }

  function getHistoryQuery() {
    var params = new URLSearchParams();
    var status = ($historyStatus.value || "").trim();
    var series = ($historySeries.value || "").trim();
    var limit = ($historyLimit.value || "25").trim();
    if (status) {
      params.set("status", status);
    }
    if (series) {
      params.set("series", series);
    }
    if ($historySince && $historySince.value) {
      var seconds = parseInt($historySince.value, 10);
      if (seconds > 0) {
        var since = Math.floor(Date.now() / 1000) - seconds;
        params.set("since", String(since));
      }
    }
    params.set("limit", limit);
    return params.toString();
  }

  function pollOcrDetails() {
    var query = getHistoryQuery();
    var path = "/queue/api/ocr" + (query ? "?" + query : "");
    fetch(path, { headers: getStatusHeaders() })
      .then(function (r) {
        if (r.status === 401) {
          window.location.href = "/login";
          return null;
        }
        return r.json();
      })
      .then(function (data) {
        if (!data) return;
        renderWorkerControls(data.ocr_worker || { available: false, paused: false });
        renderHistory(data.history || []);
      })
      .catch(function () {});
  }

  function sendControlAction(action) {
    if (isControlInFlight) {
      return;
    }
    isControlInFlight = true;
    renderWorkerControls({
      available: true,
      paused: action === "pause" ? true : false,
    });
    fetch("/queue/api/ocr/control", {
      method: "POST",
      headers: getAdminHeaders(),
      body: JSON.stringify({ action: action })
    })
      .then(function (r) {
        if (r.status === 401 || r.status === 403) {
          $ocrControlStatus.textContent = "Admin login required for OCR controls";
          showToast("Admin login required for OCR controls", "warning");
          return null;
        }
        return r.json();
      })
      .then(function (data) {
        if (!data) return;
        showToast("OCR worker " + (action === "pause" ? "paused" : "resumed"), "success");
        pollOcrDetails();
        poll();
      })
      .catch(function () {
        $ocrControlStatus.textContent = "Control request failed";
        showToast("Control request failed", "error");
      })
      .finally(function () {
        isControlInFlight = false;
      });
  }

  function escapeHtml(s) {
    var d = document.createElement("div");
    d.textContent = s;
    return d.innerHTML;
  }

  function poll() {
    fetch("/queue/api/status", { headers: getStatusHeaders() })
      .then(function (r) {
        if (r.status === 401) {
          window.location.href = "/login";
          return null;
        }
        return r.json();
      })
      .then(function (data) {
        if (!data) return;
        var backend = data.backend || "unknown";
        $backend.textContent = backend;
        applyBackendState(backend);
        renderCurrent(data.current);
        renderPendingOcr(data.pending_ocr || []);
        renderPendingThumbs(data.pending_thumbnails || 0);
        renderWorkerControls(data.ocr_worker || { available: false, paused: false });
        if ($metricActive) {
          $metricActive.textContent = data.current ? (data.current.volume || "Running") : "None";
        }
        if ($metricPendingOcr) {
          $metricPendingOcr.textContent = String((data.pending_ocr || []).length);
        }
        if ($metricPendingThumb) {
          $metricPendingThumb.textContent = String(data.pending_thumbnails || 0);
        }
        if ($lastUpdated) {
          $lastUpdated.textContent = "Updated " + new Date().toLocaleTimeString();
        }
        lastStatusUpdateMs = Date.now();
        document.body.classList.remove("queue-stale");
      })
      .catch(function () {});
  }

  function runRefreshCycle() {
    poll();
    pollOcrDetails();
  }

  function resetAutoRefresh() {
    if (pollTimerId) {
      window.clearInterval(pollTimerId);
      pollTimerId = null;
    }
    if (historyTimerId) {
      window.clearInterval(historyTimerId);
      historyTimerId = null;
    }
    if (staleTimerId) {
      window.clearInterval(staleTimerId);
      staleTimerId = null;
    }

    if (!$autoRefresh || !$autoRefresh.checked) {
      return;
    }

    var intervalMs = parseInt(($refreshInterval && $refreshInterval.value) || String(DEFAULT_POLL_INTERVAL), 10);
    if (!Number.isFinite(intervalMs) || intervalMs < 1000) {
      intervalMs = DEFAULT_POLL_INTERVAL;
    }
    pollTimerId = window.setInterval(poll, intervalMs);
    historyTimerId = window.setInterval(pollOcrDetails, intervalMs);

    staleTimerId = window.setInterval(function () {
      if (!lastStatusUpdateMs) {
        return;
      }
      var staleAfter = intervalMs * 2.5;
      var stale = (Date.now() - lastStatusUpdateMs) > staleAfter;
      document.body.classList.toggle("queue-stale", stale);
      if (stale && $lastUpdated) {
        $lastUpdated.textContent = "Stale data";
      }
    }, 1000);
  }

  function bindUiEvents() {
    if ($ocrPauseBtn) {
      $ocrPauseBtn.addEventListener("click", function () {
        sendControlAction("pause");
      });
    }
    if ($ocrResumeBtn) {
      $ocrResumeBtn.addEventListener("click", function () {
        sendControlAction("resume");
      });
    }

    var historyPoll = function () {
      pollOcrDetails();
    };
    if ($historyStatus) {
      $historyStatus.addEventListener("change", function () {
        saveUiPreferences();
        historyPoll();
      });
    }
    if ($historySeries) {
      $historySeries.addEventListener("input", function () {
        saveUiPreferences();
        historyPoll();
      });
    }
    if ($historySince) {
      $historySince.addEventListener("change", function () {
        saveUiPreferences();
        historyPoll();
      });
    }
    if ($historyLimit) {
      $historyLimit.addEventListener("change", function () {
        saveUiPreferences();
        historyPoll();
      });
    }
    if ($historyClear) {
      $historyClear.addEventListener("click", function () {
        if ($historyStatus) $historyStatus.value = "";
        if ($historySeries) $historySeries.value = "";
        if ($historySince) $historySince.value = "";
        if ($historyLimit) $historyLimit.value = "25";
        saveUiPreferences();
        historyPoll();
      });
    }
    if ($pendingSearch) {
      $pendingSearch.addEventListener("input", applyPendingFilter);
    }
    if ($refreshNow) {
      $refreshNow.addEventListener("click", function () {
        runRefreshCycle();
        showToast("Queue refreshed", "success");
      });
    }
    if ($autoRefresh) {
      $autoRefresh.addEventListener("change", function () {
        saveUiPreferences();
        resetAutoRefresh();
      });
    }
    if ($refreshInterval) {
      $refreshInterval.addEventListener("change", function () {
        saveUiPreferences();
        resetAutoRefresh();
      });
    }
  }

  function init() {
    fetch("/queue/api/config")
      .then(function (r) { return r.json(); })
      .then(function (cfg) {
        queueConfig = cfg || queueConfig;
        return updateNav();
      })
      .then(function () {
        if (!queueConfig.public_access && !getSessionAuth()) {
          window.location.href = "/login";
          return;
        }

        loadUiPreferences();
        bindUiEvents();
        runRefreshCycle();
        resetAutoRefresh();
      })
      .catch(function () {
        updateNav();
        loadUiPreferences();
        bindUiEvents();
        runRefreshCycle();
        resetAutoRefresh();
      });
  }

  window.logout = logout;
  init();
})();
