// Omni omnibox enhancement: live autocomplete dropdown over the SSR search form.
//
// Progressive enhancement — the form works without JS (plain GET to /search).
// With JS, typing fetches completions from /ac (the OpenSearch list endpoint
// Flux also consumes) and shows a glass dropdown with keyboard navigation.
//
// Authored as plain JS (no build step) but JSDoc-typed so `tsc --checkJs` and
// editors type-check it. Migrating to a Vite + TypeScript bundle later is a
// drop-in: the markup contract (#q, #suggest, form) stays the same.

/** @type {HTMLInputElement | null} */
const input = document.querySelector("#q");
/** @type {HTMLElement | null} */
const box = document.querySelector("#suggest");
/** @type {HTMLFormElement | null} */
const form = document.querySelector("#searchform");

if (input && box && form) {
  /** @type {string[]} */
  let items = [];
  let active = -1;
  let seq = 0; // guards against out-of-order responses

  const close = () => {
    box.classList.remove("open");
    box.innerHTML = "";
    items = [];
    active = -1;
  };

  const render = () => {
    if (items.length === 0) {
      close();
      return;
    }
    box.innerHTML = items
      .map(
        (s, i) =>
          `<div class="suggest-item${i === active ? " active" : ""}" data-i="${i}">` +
          `<span class="glyph">⌕</span><span>${escapeHtml(s)}</span></div>`,
      )
      .join("");
    box.classList.add("open");
  };

  const submit = (value) => {
    input.value = value;
    form.submit();
  };

  const fetchSuggest = debounce(async (q) => {
    if (!q.trim()) {
      close();
      return;
    }
    const mine = ++seq;
    try {
      const res = await fetch(`/ac?q=${encodeURIComponent(q)}`);
      const data = await res.json(); // ["query", ["suggestion", ...]]
      if (mine !== seq) return; // a newer keystroke already fired
      items = Array.isArray(data) && Array.isArray(data[1]) ? data[1] : [];
      active = -1;
      render();
    } catch {
      close();
    }
  }, 110);

  input.addEventListener("input", () => fetchSuggest(input.value));

  input.addEventListener("keydown", (e) => {
    if (!box.classList.contains("open")) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      active = (active + 1) % items.length;
      render();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      active = (active - 1 + items.length) % items.length;
      render();
    } else if (e.key === "Enter") {
      if (active >= 0) {
        e.preventDefault();
        submit(items[active]);
      }
    } else if (e.key === "Escape") {
      close();
    }
  });

  box.addEventListener("mousedown", (e) => {
    const el = /** @type {HTMLElement} */ (e.target).closest(".suggest-item");
    if (el) {
      e.preventDefault();
      submit(items[Number(el.getAttribute("data-i"))]);
    }
  });

  document.addEventListener("click", (e) => {
    if (!form.contains(/** @type {Node} */ (e.target))) close();
  });
}

/** @param {(...a: any[]) => void} fn @param {number} ms */
function debounce(fn, ms) {
  /** @type {ReturnType<typeof setTimeout>} */
  let t;
  return (/** @type {any[]} */ ...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}

/** @param {string} s */
function escapeHtml(s) {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}
