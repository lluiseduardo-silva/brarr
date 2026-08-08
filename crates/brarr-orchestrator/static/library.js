/*
 * Selection mode for the library's bulk actions.
 *
 * Two pieces of genuinely client state: whether the operator is picking
 * titles, and which ones. Neither needs to survive a reload, and asking
 * the server after each click would be a round trip to count checkboxes
 * the DOM already holds. Same call the import screen made — there the
 * DOM *is* the store, and the form carries it on submit.
 *
 * The mode lives on `<body>`, not on `#library-body`: a keystroke in the
 * search box swaps the whole list, and the operator's mode must not go
 * with it. The *selection* does go with it, which is honest — the list
 * they were picking from is no longer on screen.
 *
 * Everything is delegated on `document`, so it survives those swaps.
 */
(function () {
  "use strict";

  var ON = "1";

  function boxes() {
    return Array.prototype.slice.call(
      document.querySelectorAll("#library-bulk [data-bulk-item]")
    );
  }

  function selecting() {
    return document.body.getAttribute("data-select") === ON;
  }

  function refresh() {
    var label = document.querySelector("[data-select-label]");
    if (label) label.textContent = selecting() ? "selecionando" : "selecionar";

    var bar = document.querySelector("[data-bulk-bar]");
    if (!bar) return;

    var all = boxes();
    var picked = all.filter(function (c) { return c.checked; });

    var count = bar.querySelector("[data-bulk-count]");
    if (count) {
      count.textContent =
        picked.length === 0
          ? "nenhum selecionado"
          : picked.length === 1
          ? "1 selecionado"
          : picked.length + " selecionados";
    }

    // Disabled rather than hidden: the actions should be visible so the
    // operator knows what picking is *for*, and unusable until there is
    // something to act on.
    Array.prototype.forEach.call(
      bar.querySelectorAll("[data-bulk-action]"),
      function (b) { b.disabled = picked.length === 0; }
    );

    var master = bar.querySelector("[data-bulk-all]");
    if (master) {
      master.checked = all.length > 0 && picked.length === all.length;
      // Partial selection is neither on nor off, and without this the
      // master box lies about it.
      master.indeterminate = picked.length > 0 && picked.length < all.length;
    }
  }

  function setMode(on) {
    if (on) {
      document.body.setAttribute("data-select", ON);
    } else {
      document.body.removeAttribute("data-select");
      boxes().forEach(function (c) { c.checked = false; });
    }
    refresh();
  }

  document.addEventListener("click", function (ev) {
    var t = ev.target;
    if (!t || !t.closest) return;

    if (t.closest("[data-select-toggle]")) {
      setMode(!selecting());
      return;
    }
    if (t.closest("[data-select-off]")) {
      setMode(false);
      return;
    }

    // In selection mode the whole card is the target. Links and buttons
    // inside it have `pointer-events: none` from the stylesheet, so a
    // click lands here and cannot navigate away.
    if (!selecting()) return;
    var card = t.closest(".lib-pick");
    if (!card) return;
    var box = card.querySelector("[data-bulk-item]");
    if (!box) return;
    ev.preventDefault();
    box.checked = !box.checked;
    refresh();
  });

  document.addEventListener("change", function (ev) {
    var t = ev.target;
    if (!t || !t.hasAttribute) return;
    if (t.hasAttribute("data-bulk-all")) {
      boxes().forEach(function (c) { c.checked = t.checked; });
      refresh();
    } else if (t.hasAttribute("data-bulk-item")) {
      refresh();
    }
  });

  // A swap replaces every row, so the count has to be recomputed and the
  // buttons re-disabled. The boxes come back unchecked, which is the
  // truthful outcome.
  document.addEventListener("htmx:afterSwap", refresh);
  document.addEventListener("DOMContentLoaded", refresh);
})();
