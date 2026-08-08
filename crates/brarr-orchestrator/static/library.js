/*
 * Selection state for the library's bulk actions.
 *
 * Which boxes are ticked is genuinely client state: it changes on every
 * click, it never needs to survive a reload, and asking the server after
 * each one would be a round trip to count checkboxes the DOM already
 * holds. Same call the import screen made — there the DOM *is* the
 * store, and the form carries it on submit.
 *
 * Everything is delegated on `document`, so it keeps working after HTMX
 * swaps the list (a search keystroke replaces every row).
 */
(function () {
  "use strict";

  var FORM = "#library-bulk";

  function items() {
    return Array.prototype.slice.call(
      document.querySelectorAll(FORM + " [data-bulk-item]")
    );
  }

  function refresh() {
    var bar = document.querySelector("[data-bulk-bar]");
    if (!bar) return;

    var all = items();
    var picked = all.filter(function (c) { return c.checked; });

    // The bar is always in the DOM and hidden by CSS. Rendering it
    // conditionally would mean a request per tick.
    bar.setAttribute("data-any", picked.length > 0 ? "1" : "0");

    var label = bar.querySelector("[data-bulk-count]");
    if (label) {
      label.textContent =
        picked.length === 0
          ? "nenhum selecionado"
          : picked.length === 1
          ? "1 selecionado"
          : picked.length + " selecionados";
    }

    var master = bar.querySelector("[data-bulk-all]");
    if (master) {
      master.checked = all.length > 0 && picked.length === all.length;
      // Partial selection reads as neither on nor off, which is what the
      // indeterminate state is for — without it the master box lies.
      master.indeterminate = picked.length > 0 && picked.length < all.length;
    }
  }

  document.addEventListener("change", function (ev) {
    var t = ev.target;
    if (!t) return;

    if (t.hasAttribute && t.hasAttribute("data-bulk-all")) {
      items().forEach(function (c) { c.checked = t.checked; });
      refresh();
      return;
    }
    if (t.hasAttribute && t.hasAttribute("data-bulk-item")) {
      refresh();
    }
  });

  // A swap replaces every row, so the count has to be recomputed —
  // and the boxes come back unchecked, which is the honest outcome:
  // the list the operator was selecting from is no longer on screen.
  document.addEventListener("htmx:afterSwap", refresh);
  document.addEventListener("DOMContentLoaded", refresh);
})();
