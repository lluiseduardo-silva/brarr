// brarr — modal lifecycle.
//
// Templates return a partial that includes a top-level <dialog> when
// the operator triggers an HTMX `hx-get` aimed at `#modal-target`.
// This script auto-opens any <dialog> swapped into that slot and
// empties the slot once the dialog closes, so the next open swap
// starts from a clean DOM.

(function () {
    'use strict';

    // Two slots, because the import dialog opens a picker on top of
    // itself. They are separate elements on purpose: emptying the slot
    // on close is what keeps a re-open fresh, and with one shared slot
    // closing the picker would empty the importer underneath it —
    // taking every assignment the operator had already made.
    var SLOT_IDS = ['modal-target', 'modal-target-2'];

    function openDialogIfPresent(slot) {
        if (!slot) return;
        var dialog = slot.querySelector('dialog');
        if (!dialog) return;
        if (typeof dialog.showModal !== 'function') {
            // Browser without native <dialog> support — leave the
            // dialog visible inline. Better than nothing.
            dialog.setAttribute('open', '');
            return;
        }
        if (dialog.open) return;
        dialog.showModal();
        dialog.addEventListener('close', function () {
            // Empty the slot so a re-open re-fetches the latest
            // template (e.g. updated provider_count after CRUD).
            slot.innerHTML = '';
        }, { once: true });
    }

    document.addEventListener('htmx:afterSwap', function (evt) {
        if (evt.target && SLOT_IDS.indexOf(evt.target.id) !== -1) {
            openDialogIfPresent(evt.target);
        }
    });

    function openAll() {
        for (var i = 0; i < SLOT_IDS.length; i++) {
            openDialogIfPresent(document.getElementById(SLOT_IDS[i]));
        }
    }

    // Initial page load — if a slot already has a dialog (e.g.
    // server-rendered modal on a flow we add later) honour the same
    // contract.
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', openAll);
    } else {
        openAll();
    }
})();
