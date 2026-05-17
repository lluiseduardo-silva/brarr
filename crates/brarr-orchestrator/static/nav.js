// brarr — nav active-link highlighter.
//
// Each <a class="nav-link" data-section="…"> in base.html maps to a
// route section. On load we pick the link whose section matches the
// current pathname and tag it with `data-active="true"`; styles in
// app.css render the gradient underline + bold weight from there.
//
// Done client-side so the Rust template structs don't need to
// thread an `active_nav` field through every handler.

(function () {
    'use strict';

    var SECTION_BY_PREFIX = [
        { prefix: '/providers',     section: 'providers' },
        { prefix: '/arr-instances', section: 'arr-instances' },
        { prefix: '/profiles',      section: 'profiles' },
        { prefix: '/releases',      section: 'releases' },
        { prefix: '/searches',      section: 'releases' },
        { prefix: '/pushes',        section: 'pushes' },
        { prefix: '/',              section: 'dashboard' }
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
        var links = document.querySelectorAll('a.nav-link[data-section]');
        for (var i = 0; i < links.length; i++) {
            if (links[i].getAttribute('data-section') === section) {
                links[i].setAttribute('data-active', 'true');
            } else {
                links[i].removeAttribute('data-active');
            }
        }
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', highlight);
    } else {
        highlight();
    }
})();
