// Selector engine and element checks, evaluated in the page (or an iframe) on every poll.
// Called as ENGINE(selector, mode). Selectors, as in Playwright:
//   css (plain or css=...)   text=Foo (case-insensitive substring)   text="Foo" (exact)
//   role=button   role=button[name="Save"] (case-insensitive substring)   [name="Save"s] (exact)
// Modes: attached | visible | click | clear | changed.
((sel, mode) => {
  const norm = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const box = (el) => el.getBoundingClientRect();
  const visible = (el) => {
    const r = box(el);
    return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden';
  };
  const m = /^(css|text|role)=([\s\S]*)$/.exec(sel);
  const engine = m ? m[1] : 'css';
  const body = m ? m[2] : sel;
  // Builds the finder; throws SyntaxError on a malformed selector (caught below).
  const makeFind = () => {
  if (engine === 'css') {
    return () => document.querySelector(body);
  } else if (engine === 'text') {
    // The deepest element whose text matches wins, like Playwright.
    const exact = /^"[\s\S]*"$/.test(body);
    const want = norm(exact ? JSON.parse(body) : body);
    const lw = want.toLowerCase();
    const has = (t) => (exact ? t.includes(want) : t.toLowerCase().includes(lw));
    const is = (t) => (exact ? t === want : has(t));
    const skip = /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|HEAD)$/;
    const walk = (el) => {
      const t = norm(el.textContent);
      if (!has(t)) return null; // no descendant can match either
      for (const c of el.children) {
        if (skip.test(c.tagName)) continue;
        const r = walk(c);
        if (r) return r;
      }
      return is(t) ? el : null;
    };
    const buttons = 'input[type=button],input[type=submit],input[type=reset]';
    return () =>
      (document.body && walk(document.body)) ||
      [...document.querySelectorAll(buttons)].find((i) => is(norm(i.value))) ||
      null;
  } else {
    const r = /^([\w-]+)(?:\[name=("(?:[^"\\]|\\.)*")(s?)\])?$/.exec(body);
    if (!r) throw new SyntaxError('expected role=button or role=button[name="Save"], got ' + body);
    const want = r[1].toLowerCase();
    const name = r[2] === undefined ? null : norm(JSON.parse(r[2]));
    const exact = r[3] === 's';
    const implicit = (el) => {
      const t = el.tagName;
      const type = (el.getAttribute('type') || '').toLowerCase();
      if (t === 'A' || t === 'AREA') return el.hasAttribute('href') ? 'link' : null;
      if (t === 'BUTTON' || t === 'SUMMARY') return 'button';
      if (t === 'INPUT') {
        if (['button', 'submit', 'reset', 'image'].includes(type)) return 'button';
        if (type === 'checkbox' || type === 'radio') return type;
        if (type === 'range') return 'slider';
        if (type === 'number') return 'spinbutton';
        if (type === 'search') return el.hasAttribute('list') ? 'combobox' : 'searchbox';
        if (['', 'text', 'email', 'tel', 'url'].includes(type)) {
          return el.hasAttribute('list') ? 'combobox' : 'textbox';
        }
        return null;
      }
      if (t === 'TEXTAREA') return 'textbox';
      if (t === 'SELECT') return el.multiple || el.size > 1 ? 'listbox' : 'combobox';
      if (/^H[1-6]$/.test(t)) return 'heading';
      if (t === 'IMG') return el.getAttribute('alt') === '' ? 'presentation' : 'img';
      const byTag = {
        OPTION: 'option', UL: 'list', OL: 'list', LI: 'listitem', NAV: 'navigation', MAIN: 'main',
        HEADER: 'banner', FOOTER: 'contentinfo', ASIDE: 'complementary', FORM: 'form',
        TABLE: 'table', TR: 'row', TD: 'cell', TH: 'columnheader', DIALOG: 'dialog',
        ARTICLE: 'article', HR: 'separator', PROGRESS: 'progressbar', FIELDSET: 'group',
      };
      return byTag[t] || null;
    };
    const roleOf = (el) =>
      (el.getAttribute('role') || '').trim().split(/\s+/)[0].toLowerCase() || implicit(el);
    const nameFromContent =
      /^(button|link|heading|cell|columnheader|option|listitem|tab|menuitem|checkbox|radio|switch|treeitem|row)$/;
    // A compact accessible-name computation: aria-labelledby, aria-label, <label>, value of
    // input buttons, alt, then text content for roles named by content, placeholder, title.
    const nameOf = (el) => {
      const ids = el.getAttribute('aria-labelledby');
      if (ids) {
        const n = norm(ids.split(/\s+/).map((id) => document.getElementById(id)?.textContent).join(' '));
        if (n) return n;
      }
      const label = norm(el.getAttribute('aria-label'));
      if (label) return label;
      if (el.labels && el.labels.length) return norm([...el.labels].map((l) => l.textContent).join(' '));
      if (el.tagName === 'INPUT' && /^(button|submit|reset)$/i.test(el.type)) {
        return norm(el.value) || { submit: 'Submit', reset: 'Reset' }[el.type] || '';
      }
      if (el.tagName === 'IMG' || el.tagName === 'AREA' || (el.tagName === 'INPUT' && el.type === 'image')) {
        const alt = norm(el.getAttribute('alt'));
        if (alt) return alt;
      }
      if (nameFromContent.test(roleOf(el) || '')) {
        const t = norm(el.textContent);
        if (t) return t;
      }
      return norm(el.getAttribute('placeholder')) || norm(el.getAttribute('title'));
    };
    const nameOk = (n) =>
      name === null || (exact ? n === name : n.toLowerCase().includes(name.toLowerCase()));
    // Hidden elements are not in the accessibility tree, so getByRole never matches them.
    const rendered = (el) =>
      el.getClientRects().length > 0 &&
      getComputedStyle(el).visibility !== 'hidden' &&
      !el.closest('[aria-hidden="true"]');
    return () =>
      [...document.querySelectorAll('*')].find(
        (el) => roleOf(el) === want && rendered(el) && nameOk(nameOf(el)),
      ) || null;
  }
  };

  let el;
  try {
    el = makeFind()();
  } catch (e) {
    return { ok: false, fatal: true, reason: 'invalid selector: ' + e.message };
  }
  if (!el) return { ok: false, reason: 'no element matches selector' };
  if (mode === 'attached') return { ok: true, text: el.textContent };
  if (mode === 'clear') {
    el.focus();
    if ('value' in el) el.value = '';
    return { ok: true };
  }
  if (mode === 'changed') {
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return { ok: true };
  }
  if (!visible(el)) return { ok: false, reason: 'hidden' };
  if (mode === 'visible') return { ok: true, text: el.textContent };
  // click: enabled, scrolled into view, and the topmost element at its centre.
  if (el.disabled === true || el.getAttribute('aria-disabled') === 'true') {
    return { ok: false, reason: 'disabled' };
  }
  el.scrollIntoViewIfNeeded(true);
  const r = box(el);
  const x = r.x + r.width / 2;
  const y = r.y + r.height / 2;
  const hit = document.elementFromPoint(x, y);
  if (!hit) return { ok: false, reason: 'centre is outside the viewport' };
  if (hit !== el && !el.contains(hit)) {
    return { ok: false, reason: 'obscured by <' + hit.tagName.toLowerCase() + (hit.id ? '#' + hit.id : '') + '>' };
  }
  return { ok: true, x, y };
})
