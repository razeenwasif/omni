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

  async function tick() {
    try {
      const r = await fetch("/stats", { cache: "no-store" });
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      const s = await r.json();
      renderCards(s);
      renderSegments(s);
      renderTop(s);
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
