// Releases page client-side filter + sort.
//
// The grid is server-rendered (one <article data-release-card> per row);
// this script keeps the page interactive without round-tripping to the
// backend for state transitions that fit comfortably in the DOM:
//
//   * Filter buttons (data-filter="all|kept|rejected") toggle `hidden`
//     on cards based on their data-state attribute. Active button
//     carries data-active="1" so the stylesheet can highlight it.
//   * Sort <select data-sort> reorders the grid in place via
//     `appendChild` — the browser preserves CSS grid placement as the
//     children change order.
//   * When the filter empties the grid, an inline message
//     (data-releases-empty) toggles visible.

(function () {
  const grid = document.querySelector("[data-releases-grid]");
  if (!grid) return;
  const cards = Array.from(grid.querySelectorAll("[data-release-card]"));
  const filterButtons = Array.from(document.querySelectorAll("[data-filter]"));
  const sortSelect = document.querySelector("[data-sort]");
  const emptyMessage = document.querySelector("[data-releases-empty]");
  let currentFilter = "all";

  function paintFilterButtons() {
    for (const btn of filterButtons) {
      const active = btn.dataset.filter === currentFilter;
      btn.dataset.active = active ? "1" : "";
    }
  }

  function applyFilter() {
    let visible = 0;
    for (const card of cards) {
      const state = card.dataset.state || "kept";
      const match = currentFilter === "all" || currentFilter === state;
      card.hidden = !match;
      if (match) visible += 1;
    }
    if (emptyMessage) emptyMessage.hidden = visible !== 0;
  }

  function applySort() {
    if (!sortSelect) return;
    const mode = sortSelect.value;
    const sorted = cards.slice().sort((a, b) => {
      const sa = Number(a.dataset.score || 0);
      const sb = Number(b.dataset.score || 0);
      return mode === "score-asc" ? sa - sb : sb - sa;
    });
    for (const card of sorted) grid.appendChild(card);
  }

  for (const btn of filterButtons) {
    btn.addEventListener("click", () => {
      currentFilter = btn.dataset.filter || "all";
      paintFilterButtons();
      applyFilter();
    });
  }
  if (sortSelect) sortSelect.addEventListener("change", applySort);

  paintFilterButtons();
  applyFilter();
})();
