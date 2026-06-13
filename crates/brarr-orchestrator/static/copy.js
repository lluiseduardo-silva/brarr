// brarr — copy-to-clipboard for `[data-copy-target]` buttons.
//
// Delegated on `document` so it also works for HTMX-swapped content
// (the *arr list re-renders on every toggle). The target selector
// points at an <input>/<textarea> (uses `.value`) or any element
// (uses `.textContent`). Shows a transient "copiado!" on the button.

(function () {
    'use strict';

    function flash(btn) {
        var prev = btn.textContent;
        btn.textContent = 'copiado!';
        setTimeout(function () { btn.textContent = prev; }, 1200);
    }

    function fallbackCopy(el) {
        try {
            if (el.select) { el.select(); }
            document.execCommand('copy');
            return true;
        } catch (e) {
            return false;
        }
    }

    document.addEventListener('click', function (ev) {
        var btn = ev.target.closest('[data-copy-target]');
        if (!btn) { return; }
        var el = document.querySelector(btn.getAttribute('data-copy-target'));
        if (!el) { return; }
        var text = (el.value !== undefined && el.value !== null) ? el.value : el.textContent;
        if (navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(text).then(
                function () { flash(btn); },
                function () { if (fallbackCopy(el)) { flash(btn); } }
            );
        } else if (fallbackCopy(el)) {
            flash(btn);
        }
    });
})();
