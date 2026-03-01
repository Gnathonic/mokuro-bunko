(function () {
  "use strict";

  const POLL_INTERVAL = 3000;

  const $backend = document.getElementById("backend");
  const $currentJob = document.getElementById("current-job");
  const $pendingOcrCount = document.getElementById("pending-ocr-count");
  const $pendingOcrList = document.getElementById("pending-ocr-list");
  const $pendingThumbCount = document.getElementById("pending-thumb-count");
  const $pendingThumbText = document.getElementById("pending-thumb-text");

  let queueConfig = { show_in_nav: false, public_access: true };

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
      (eta ? '<span class="current-job__eta">ETA ' + eta + "</span>" : "") +
      "</div>";
  }

  function renderPendingOcr(list) {
    $pendingOcrCount.textContent = list.length;
    if (!list.length) {
      $pendingOcrList.innerHTML =
        '<p class="pending-list__empty">No volumes waiting for OCR</p>';
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
        $backend.textContent = data.backend || "unknown";
        renderCurrent(data.current);
        renderPendingOcr(data.pending_ocr || []);
        renderPendingThumbs(data.pending_thumbnails || 0);
      })
      .catch(function () {});
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

        poll();
        setInterval(poll, POLL_INTERVAL);
      })
      .catch(function () {
        updateNav();
        poll();
        setInterval(poll, POLL_INTERVAL);
      });
  }

  window.logout = logout;
  init();
})();
