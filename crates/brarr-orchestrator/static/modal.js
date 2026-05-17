// brarr — modal lifecycle.
//
// Templates return a partial that includes a top-level <dialog> when
// the operator triggers an HTMX `hx-get` aimed at `#modal-target`.
// This script auto-opens any <dialog> swapped into that slot and
// empties the slot once the dialog closes, so the next open swap
// starts from a clean DOM.

(function () {
    'use strict';

    var SLOT_ID = 'modal-target';

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
        if (evt.target && evt.target.id === SLOT_ID) {
            openDialogIfPresent(evt.target);
        }
    });

    // Initial page load — if the slot already has a dialog (e.g.
    // server-rendered modal on a flow we add later) honour the same
    // contract.
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function () {
            openDialogIfPresent(document.getElementById(SLOT_ID));
        });
    } else {
        openDialogIfPresent(document.getElementById(SLOT_ID));
    }
})();
