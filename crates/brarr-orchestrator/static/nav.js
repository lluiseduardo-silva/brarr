// brarr — nav active-link highlighter + dropdown behaviour.
//
// Each <a class="nav-link" data-section="…"> in base.html maps to a
// route section. On load we pick the link whose section matches the
// current pathname and tag it with `data-active="true"`; styles in
// app.css render the gradient underline + bold weight from there.
//
// Links that live inside a <details class="nav-menu"> dropdown also
// mark their parent summary, so "Configuração" reads as active while
// you are on /providers.
//
// Done client-side so the Rust template structs don't need to thread an
// `active_nav` field through every handler.

(function () {
    'use strict';

    // Scanned in order; '/' is the catch-all and stays last.
    var SECTION_BY_PREFIX = [
        { prefix: '/providers',        section: 'providers' },
        { prefix: '/download-clients', section: 'download-clients' },
        { prefix: '/root-folders',     section: 'download-clients' },
        { prefix: '/queue',            section: 'queue' },
        { prefix: '/arr-instances',    section: 'arr-instances' },
        { prefix: '/library',          section: 'library' },
        { prefix: '/profiles',         section: 'profiles' },
        { prefix: '/releases',         section: 'releases' },
        { prefix: '/searches',         section: 'searches' },
        { prefix: '/pushes',           section: 'pushes' },
        { prefix: '/webhooks',         section: 'webhooks' },
        { prefix: '/health',           section: 'health' },
        { prefix: '/settings',         section: 'settings' },
        { prefix: '/',                 section: 'dashboard' }
    ];

    function detectSection(pathname) {
        for (var i = 0; i < SECTION_BY_PREFIX.length; i++) {
            if (pathname === SECTION_BY_PREFIX[i].prefix ||
                pathname.indexOf(SECTION_BY_PREFIX[i].prefix + '/') === 0) {
                return SECTION_BY_PREFIX[i].section;
            }
        }
        return null;
    }

    function highlight() {
        var section = detectSection(window.location.pathname);
        if (!section) return;
        var links = document.querySelectorAll('[data-section]');
        for (var i = 0; i < links.length; i++) {
            var el = links[i];
            if (el.getAttribute('data-section') === section) {
                el.setAttribute('data-active', 'true');
                // A link inside a dropdown also lights its own menu, so
                // the top bar still says where you are.
                var menu = el.closest ? el.closest('.nav-menu') : null;
                if (menu) {
                    var summary = menu.querySelector('summary');
                    if (summary) summary.setAttribute('data-active', 'true');
                }
            } else if (!el.querySelector || !el.querySelector('[data-section]')) {
                el.removeAttribute('data-active');
            }
        }
    }

    // One menu open at a time, and a click anywhere else closes it —
    // <details> gives us the toggle for free but neither of these.
    function wireDropdowns() {
        var menus = document.querySelectorAll('details.nav-menu');
        for (var i = 0; i < menus.length; i++) {
            menus[i].addEventListener('toggle', function () {
                if (!this.open) return;
                for (var j = 0; j < menus.length; j++) {
                    if (menus[j] !== this) menus[j].removeAttribute('open');
                }
            });
        }
        document.addEventListener('click', function (event) {
            for (var i = 0; i < menus.length; i++) {
                if (!menus[i].contains(event.target)) {
                    menus[i].removeAttribute('open');
                }
            }
        });
        document.addEventListener('keydown', function (event) {
            if (event.key !== 'Escape') return;
            for (var i = 0; i < menus.length; i++) {
                menus[i].removeAttribute('open');
            }
        });
    }

    function init() {
        highlight();
        wireDropdowns();
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init);
    } else {
        init();
    }
})();
