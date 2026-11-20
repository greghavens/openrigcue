/* OpenRigCue interface: the browser side.
 *
 * The program draws the page; this script sends the user's actions back to it and gives the page its
 * motion. Every element with `data-act` sends its action name, its `data-arg` and its value (a checkbox
 * sends 1 or 0) to the program's action receiver. Range sliders show their value while they move and send
 * it when released; fields marked `data-live` send their text as it is typed. After each redraw the script
 * opens dialogs as modal, places and focuses menus, and times the toasts: a toast with `data-life` leaves
 * after that many milliseconds, paused while it is hovered or focused, and the program is told when it
 * leaves.
 */
(function () {
  'use strict';

  var RECEIVER = 'orc-act';

  function send(act, arg, value) {
    if (!window.remi || !act) return;
    var params = { act: act };
    if (arg !== undefined && arg !== null) params.arg = String(arg);
    if (value !== undefined && value !== null) params.value = String(value);
    window.remi.sendCallbackParam(RECEIVER, 'onact', params);
  }

  function attr(el, name) {
    var value = el.getAttribute(name);
    return value === null ? undefined : value;
  }

  // ---- Menus ----

  function openMenu() {
    return document.querySelector('.language-menu[data-anchor]');
  }

  function closeMenuOnOutsideClick(target) {
    var menu = openMenu();
    if (!menu) return;
    var anchor = document.getElementById(menu.getAttribute('data-anchor'));
    if (menu.contains(target) || (anchor && anchor.contains(target))) return;
    send('popup-close');
  }

  function placeMenu(menu) {
    var anchor = document.getElementById(menu.getAttribute('data-anchor'));
    if (!anchor) return;
    var rect = anchor.getBoundingClientRect();
    menu.style.top = rect.bottom + 8 + 'px';
    menu.style.right = Math.max(8, window.innerWidth - rect.right) + 'px';
  }

  // ---- Actions ----

  document.addEventListener('click', function (event) {
    closeMenuOnOutsideClick(event.target);
    var el = event.target.closest ? event.target.closest('[data-act]') : null;
    if (!el || el.disabled) return;
    var tag = el.tagName;
    if (tag === 'SELECT' || tag === 'DETAILS' || tag === 'TEXTAREA') return;
    if (tag === 'INPUT' && el.type !== 'button' && el.type !== 'submit') return;
    event.preventDefault();
    send(attr(el, 'data-act'), attr(el, 'data-arg'), attr(el, 'data-value'));
  });

  function pairValues(el) {
    var id = el.getAttribute('data-pair');
    var low = document.getElementById(id);
    var high = document.getElementById(id + '-high');
    if (!low || !high) return null;
    var lo = +low.value;
    var hi = +high.value;
    if (el === low && lo > hi) { low.value = hi; lo = hi; }
    if (el === high && hi < lo) { high.value = lo; hi = lo; }
    return [lo, hi];
  }

  function showValue(el) {
    var out = document.getElementById(el.getAttribute('data-output') || '');
    if (!out) return;
    var template = el.getAttribute('data-template') || '{}';
    var text = template;
    if (el.hasAttribute('data-pair')) {
      var pair = pairValues(el);
      if (pair) text = text.replace('{lo}', pair[0]).replace('{hi}', pair[1]);
    }
    out.textContent = text.replace('{}', el.value);
  }

  // A live field's text as typed: a redraw that arrives before the program has seen the last keys keeps it.
  var typing = {};
  var typed = {};
  document.addEventListener('input', function (event) {
    var el = event.target;
    if (el.type === 'range') showValue(el);
    if (el.hasAttribute && el.hasAttribute('data-live') && el.hasAttribute('data-act')) {
      var key = el.id || attr(el, 'data-act');
      typed[key] = el.value;
      clearTimeout(typing[key]);
      typing[key] = setTimeout(function () {
        send(attr(el, 'data-act'), attr(el, 'data-arg'), el.value);
      }, 180);
    }
  });

  function readFile(el) {
    var file = el.files && el.files[0];
    if (!file) return;
    var reader = new FileReader();
    reader.onload = function () {
      send(attr(el, 'data-act'), file.name, reader.result);
      el.value = '';
    };
    reader.readAsDataURL(file);
  }

  document.addEventListener('change', function (event) {
    var el = event.target;
    if (!el.hasAttribute || !el.hasAttribute('data-act') || el.hasAttribute('data-live')) return;
    if (el.type === 'file') { readFile(el); return; }
    var value = el.type === 'checkbox' ? (el.checked ? '1' : '0') : el.value;
    if (el.hasAttribute('data-pair')) pairValues(el);
    send(attr(el, 'data-act'), attr(el, 'data-arg'), el.type === 'range' ? el.value : value);
  });

  // Disclosures report whether they are open, so a redraw keeps them so.
  document.addEventListener('toggle', function (event) {
    var el = event.target;
    if (el.tagName === 'DETAILS' && el.hasAttribute('data-act')) {
      send(attr(el, 'data-act'), attr(el, 'data-arg'), el.open ? '1' : '0');
    }
  }, true);

  // ---- Keys ----

  document.addEventListener('keydown', function (event) {
    var el = event.target;
    if (el.getAttribute && el.getAttribute('role') === 'tab') {
      var keys = ['ArrowLeft', 'ArrowRight', 'Home', 'End'];
      if (keys.indexOf(event.key) < 0) return;
      event.preventDefault();
      var tabs = Array.prototype.slice.call(el.parentNode.querySelectorAll('[role=tab]'));
      var index = tabs.indexOf(el);
      var next = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1
        : (index + (event.key === 'ArrowRight' ? 1 : tabs.length - 1)) % tabs.length;
      focusAfterDraw = tabs[next].id;
      tabs[next].click();
      return;
    }
    var menu = openMenu();
    if (menu && menu.contains(el)) {
      if (event.key === 'Escape') {
        event.preventDefault();
        send('popup-close');
        var anchor = document.getElementById(menu.getAttribute('data-anchor'));
        if (anchor) anchor.focus();
        return;
      }
      if (['ArrowDown', 'ArrowUp', 'Home', 'End'].indexOf(event.key) >= 0 && el.tagName === 'BUTTON') {
        event.preventDefault();
        var items = Array.prototype.slice.call(menu.querySelectorAll('button'));
        var i = items.indexOf(el);
        var target = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1
          : (i + (event.key === 'ArrowDown' ? 1 : items.length - 1)) % items.length;
        items[target].focus();
      }
      return;
    }
    if (event.key === 'Escape') {
      var drawer = document.getElementById('activity-drawer');
      if (drawer && !drawer.hidden) {
        send('drawer-close');
        var button = document.querySelector('.activity-button');
        if (button) button.focus();
      }
    }
  });

  window.addEventListener('resize', function () {
    if (document.getElementById('language-menu')) send('popup-close');
  });
  window.addEventListener('scroll', function () {
    if (document.getElementById('language-menu')) send('popup-close');
  }, { passive: true });

  // ---- Toasts ----

  var toasts = {};

  function startToast(id) {
    var t = toasts[id];
    clearTimeout(t.timer);
    t.started = Date.now();
    t.timer = setTimeout(function () {
      delete toasts[id];
      var el = document.querySelector('[data-toast="' + id + '"]');
      if (el) el.remove();
      send('toast-expire', id);
    }, Math.max(0, t.remaining));
  }

  function pauseToast(id) {
    var t = toasts[id];
    if (!t || t.paused) return;
    clearTimeout(t.timer);
    t.remaining -= Date.now() - t.started;
    t.paused = true;
  }

  function resumeToast(id) {
    var t = toasts[id];
    if (!t || !t.paused) return;
    t.paused = false;
    startToast(id);
  }

  function settleToasts() {
    var seen = {};
    var cards = document.querySelectorAll('.toast[data-toast]');
    Array.prototype.forEach.call(cards, function (el) {
      var id = el.getAttribute('data-toast');
      var life = +el.getAttribute('data-life');
      seen[id] = true;
      if (!(life > 0)) return;
      if (!toasts[id]) {
        toasts[id] = { remaining: life, started: Date.now(), timer: null, paused: false };
        startToast(id);
      }
      if (!el.orcTimed) {
        el.orcTimed = true;
        el.addEventListener('mouseenter', function () { pauseToast(id); });
        el.addEventListener('focusin', function () { pauseToast(id); });
        el.addEventListener('mouseleave', function () { if (!el.contains(document.activeElement)) resumeToast(id); });
        el.addEventListener('focusout', function (e) { if (!el.contains(e.relatedTarget) && !el.matches(':hover')) resumeToast(id); });
        if (el.matches(':hover') || el.contains(document.activeElement)) pauseToast(id);
      }
    });
    Object.keys(toasts).forEach(function (id) {
      if (!seen[id]) { clearTimeout(toasts[id].timer); delete toasts[id]; }
    });
  }

  // ---- After each redraw ----

  var focusAfterDraw = null;
  var openedMenu = null;
  var drawerWasOpen = false;

  function settleDialog() {
    var dialog = document.querySelector('dialog[data-modal]');
    if (!dialog) return;
    if (!dialog.open) {
      try { dialog.showModal(); } catch (e) { dialog.setAttribute('open', ''); }
      var first = dialog.querySelector('[data-autofocus], .primary:not([disabled]), select, input, button');
      if (first) first.focus();
    }
    if (!dialog.orcBound) {
      dialog.orcBound = true;
      dialog.addEventListener('cancel', function (event) {
        event.preventDefault();
        if (!dialog.hasAttribute('data-locked')) send('dialog-close');
      });
    }
  }

  function settleMenu() {
    var menu = openMenu();
    if (!menu) { openedMenu = null; return; }
    placeMenu(menu);
    if (openedMenu !== menu.id) {
      openedMenu = menu.id;
      var target = menu.querySelector('[data-autofocus], [aria-checked="true"], button');
      if (target) {
        target.focus();
        if (target.setSelectionRange && target.value) target.setSelectionRange(target.value.length, target.value.length);
      }
    }
  }

  function settleDrawer() {
    var drawer = document.getElementById('activity-drawer');
    var open = !!drawer && !drawer.hidden;
    if (open && !drawerWasOpen) {
      var close = document.getElementById('close-drawer');
      if (close) close.focus();
    }
    drawerWasOpen = open;
  }

  function settleTyped() {
    Object.keys(typed).forEach(function (id) {
      var el = document.getElementById(id);
      if (!el) { delete typed[id]; return; }
      if (el.value !== typed[id]) {
        el.value = typed[id];
        if (document.activeElement === el && el.setSelectionRange) el.setSelectionRange(el.value.length, el.value.length);
      }
    });
  }

  function settle() {
    settleTyped();
    settleDialog();
    settleMenu();
    settleDrawer();
    settleToasts();
    if (focusAfterDraw) {
      var el = document.getElementById(focusAfterDraw);
      if (el) { el.focus(); focusAfterDraw = null; }
    }
  }

  var pending = false;
  function schedule() {
    if (pending) return;
    pending = true;
    window.requestAnimationFrame(function () { pending = false; settle(); });
  }

  function watch() {
    new MutationObserver(schedule).observe(document.body, { childList: true, subtree: true });
    schedule();
  }

  if (document.body) watch();
  else document.addEventListener('DOMContentLoaded', watch);
})();
