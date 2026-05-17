// brarr — theme toggle.
//
// Sets a `data-theme="light"|"dark"` attribute on <html>. The
// app.css design tokens swap automatically because every colour
// utility resolves through CSS custom properties scoped to that
// attribute (see styles/input.css).
//
// Persistence: `brarr_theme` cookie (1 year, SameSite=Lax). The
// initial value runs synchronously in the <head> to avoid a flash
// of the wrong theme on first paint.

(function () {
    'use strict';

    var COOKIE = 'brarr_theme';
    var ATTR = 'data-theme';

    function readCookie(name) {
        var pairs = document.cookie ? document.cookie.split('; ') : [];
        for (var i = 0; i < pairs.length; i++) {
            var idx = pairs[i].indexOf('=');
            if (idx > -1 && pairs[i].slice(0, idx) === name) {
                return decodeURIComponent(pairs[i].slice(idx + 1));
            }
        }
        return null;
    }

    function writeCookie(name, value) {
        var oneYear = 60 * 60 * 24 * 365;
        document.cookie =
            name + '=' + encodeURIComponent(value) +
            '; Max-Age=' + oneYear + '; Path=/; SameSite=Lax';
    }

    function systemPrefersDark() {
        return window.matchMedia &&
            window.matchMedia('(prefers-color-scheme: dark)').matches;
    }

    function apply(theme) {
        if (theme === 'light' || theme === 'dark') {
            document.documentElement.setAttribute(ATTR, theme);
        } else {
            document.documentElement.removeAttribute(ATTR);
        }
    }

    // Resolve the initial theme. Saved preference wins; otherwise we
    // let prefers-color-scheme drive (no attribute = CSS fallback).
    var saved = readCookie(COOKIE);
    if (saved === 'light' || saved === 'dark') {
        apply(saved);
    }

    // Expose a tiny API for the toggle button to call. Cycles
    // explicit Light -> Dark -> System (clears cookie + attribute).
    window.brarrTheme = {
        current: function () {
            var a = document.documentElement.getAttribute(ATTR);
            if (a === 'light' || a === 'dark') return a;
            return systemPrefersDark() ? 'dark' : 'light';
        },
        set: function (theme) {
            if (theme === 'system') {
                writeCookie(COOKIE, '');
                apply(null);
            } else if (theme === 'light' || theme === 'dark') {
                writeCookie(COOKIE, theme);
                apply(theme);
            }
        },
        toggle: function () {
            var next = this.current() === 'dark' ? 'light' : 'dark';
            this.set(next);
        }
    };

    // Re-render system preference if the user changes their OS theme
    // and we are in "follow system" mode (no explicit cookie).
    if (window.matchMedia) {
        var mql = window.matchMedia('(prefers-color-scheme: dark)');
        var onChange = function () {
            if (!readCookie(COOKIE)) {
                // No-op for attribute (still absent); the CSS @media
                // rule re-evaluates on its own. We just need any
                // listener attached so Safari fires `change`.
            }
        };
        if (mql.addEventListener) {
            mql.addEventListener('change', onChange);
        } else if (mql.addListener) {
            mql.addListener(onChange);
        }
    }
})();
