// brarr — client-side datetime formatter.
//
// Templates render timestamps inside <time data-datetime="ISO-8601">
// tags. This script walks the DOM (initial load + after every HTMX
// swap) and rewrites the inner text in the user's locale + timezone
// via Intl.DateTimeFormat. The server keeps emitting UTC strings —
// rendering decisions belong to the client that knows the operator's
// timezone.
//
// Marker attributes the templates use:
//   <time data-datetime="2025-11-04T14:32:18Z" data-datetime-style="long">
// The optional `data-datetime-style` switches between `short`
// (default, "04/11/25 11:32") and `long` ("4 nov 2025, 11:32:18").

(function () {
    'use strict';

    var SELECTOR = 'time[data-datetime]';

    function fmt(style) {
        var opts = style === 'long'
            ? { dateStyle: 'medium', timeStyle: 'medium' }
            : { dateStyle: 'short',  timeStyle: 'short'  };
        try {
            return new Intl.DateTimeFormat(undefined, opts);
        } catch (_) {
            return null;
        }
    }
    var SHORT = fmt('short');
    var LONG  = fmt('long');

    function format(elem) {
        var iso = elem.getAttribute('data-datetime');
        if (!iso) return;
        var d = new Date(iso);
        if (isNaN(d.getTime())) return;
        var style = elem.getAttribute('data-datetime-style') === 'long' ? LONG : SHORT;
        if (!style) return;
        try {
            elem.textContent = style.format(d);
            // Also set the title so hover reveals the raw UTC string —
            // helps when comparing logs across machines.
            if (!elem.hasAttribute('title')) {
                elem.setAttribute('title', iso);
            }
        } catch (_) {
            // Leave the server-rendered fallback in place.
        }
    }

    function formatAll(root) {
        var nodes = (root || document).querySelectorAll(SELECTOR);
        for (var i = 0; i < nodes.length; i++) format(nodes[i]);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function () { formatAll(); });
    } else {
        formatAll();
    }

    document.addEventListener('htmx:afterSwap', function (evt) {
        formatAll(evt.target || document);
    });
})();
