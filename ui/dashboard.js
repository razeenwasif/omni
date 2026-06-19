/*
 * Omni dashboard — fetches /stats and renders the live index health view,
 * refreshing every couple seconds so a background merge is visible in real time.
 * Vanilla JS, no build step (baked into the binary via include_str!).
 */
(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const esc = (s) =>
    String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

  const REFRESH_MS = 2000;
  let lastOk = 0;

  // The headline stat cards.
  function renderCards(s) {
    const embedded = s.embedded
      ? `on · ${esc(s.embedder_kind)} ${s.embedder_dim}d`
      : "off";
    const ann = s.ann ? `${s.ann_vectors.toLocaleString()} vec` : "—";
    const cards = [
      { label: "Live documents", num: s.live_docs.toLocaleString(), sub: `${s.total_docs.toLocaleString()} on disk` },
      { label: "Segments", num: s.segments, sub: "merge-balanced", accent: true },
      { label: "Tombstones", num: s.tombstones.toLocaleString(), sub: "reclaim on merge" },
      { label: "Searches", num: (s.searches || 0).toLocaleString(), sub: `${(s.clicks || 0).toLocaleString()} clicks` },
      { label: "Zero results", num: `${Math.round((s.zero_result_rate || 0) * 100)}%`, sub: `${(s.zero_results || 0).toLocaleString()} queries`, small: true },
      { label: "Latency", num: `${Math.round(s.p95_search_ms || 0)} ms`, sub: `${Math.round(s.avg_search_ms || 0)} ms avg`, small: true },
      { label: "Embeddings", num: embedded, sub: "semantic / hybrid", small: true },
      { label: "ANN (HNSW)", num: ann, sub: s.ann ? "sub-linear recall" : "brute-force", small: true },
      { label: "Avg length", num: `${Math.round(s.avg_body_len)}`, sub: `body · ${Math.round(s.avg_title_len)} title`, small: true },
    ];
    $("cards").innerHTML = cards
      .map(
        (c) =>
          `<div class="card${c.accent ? " accent" : ""}">` +
          `<div class="card-num${c.small ? " sm" : ""}">${esc(c.num)}</div>` +
          `<div class="card-label">${esc(c.label)}</div>` +
          `<div class="card-sub">${esc(c.sub)}</div></div>`
      )
      .join("");
  }

  // One bar per segment, width proportional to its doc count; the live portion is
  // filled, tombstones shown as a dimmer remainder.
  function renderSegments(s) {
    const segs = s.segment_sizes || [];
    const max = segs.reduce((m, x) => Math.max(m, x.total), 1);
    $("segnote").textContent = `${segs.length} · largest ${max.toLocaleString()} docs`;
    $("segbars").innerHTML = segs
      .map((x, i) => {
        const w = Math.max(2, (x.total / max) * 100);
        const livePct = x.total ? (x.live / x.total) * 100 : 0;
        return (
          `<div class="segrow">` +
          `<span class="segidx">#${i}</span>` +
          `<span class="segbar" style="width:${w}%"><span class="segfill" style="width:${livePct}%"></span></span>` +
          `<span class="segn">${x.live.toLocaleString()}${x.live !== x.total ? ` / ${x.total.toLocaleString()}` : ""}</span>` +
          `</div>`
        );
      })
      .join("");
  }

  function renderTop(s) {
    const top = s.top_docs || [];
    if (!top.some((d) => d.rank > 0)) {
      $("toplist").innerHTML = `<li class="empty">No PageRank yet (no link graph).</li>`;
      return;
    }
    $("toplist").innerHTML = top
      .map(
        (d) =>
          `<li><a href="${esc(d.url)}" class="top-title">${esc(d.title || d.url)}</a>` +
          `<span class="top-rank">${d.rank.toFixed(4)}</span>` +
          `<div class="top-url">${esc(d.url)}</div></li>`
      )
      .join("");
  }

  function renderTelemetry(s) {
    const queries = s.top_queries || [];
    $("querylist").innerHTML = queries.length
      ? queries
          .map(
            (q) =>
              `<li><span class="top-title">${esc(q.query)}</span>` +
              `<span class="top-rank">${q.count.toLocaleString()}</span>` +
              `<div class="top-url">${q.zero_results.toLocaleString()} zero · ${q.avg_results.toFixed(1)} results · ${q.avg_ms.toFixed(1)} ms</div></li>`
          )
          .join("")
      : `<li class="empty">No searches recorded this session.</li>`;

    const clicks = s.top_clicks || [];
    $("clicklist").innerHTML = clicks.length
      ? clicks
          .map(
            (c) =>
              `<li><a href="${esc(c.url)}" class="top-title">${esc(c.url)}</a>` +
              `<span class="top-rank">${c.clicks.toLocaleString()}</span>` +
              `<div class="top-url">${c.last_query ? `last query: ${esc(c.last_query)}` : "direct click"}</div></li>`
          )
          .join("")
      : `<li class="empty">No result clicks recorded this session.</li>`;
  }

  async function tick() {
    try {
      const r = await fetch("/stats", { cache: "no-store" });
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      const s = await r.json();
      renderCards(s);
      renderSegments(s);
      renderTop(s);
      renderTelemetry(s);
      lastOk = Date.now();
      $("foot").innerHTML = `<span class="dot"></span> live · refreshed every ${REFRESH_MS / 1000}s`;
    } catch (e) {
      const ago = lastOk ? `${Math.round((Date.now() - lastOk) / 1000)}s ago` : "never";
      $("foot").textContent = `offline — last update ${ago} (${e.message})`;
    }
  }

  tick();
  setInterval(tick, REFRESH_MS);
})();
