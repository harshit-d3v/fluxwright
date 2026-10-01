// Selector engine and element checks, evaluated in the page (or an iframe) on every poll.
// Called as ENGINE(selector, mode). Selectors, as in Playwright:
//   css (plain or css=...)   text=Foo or text="Foo"i (case-insensitive substring)   text="Foo" (exact)
//   role=button   role=button[name="Save"] (case-insensitive substring)   [name="Save"s] (exact)
//   label=... and placeholder=... (matched like text=)   testid="id" (data-testid, exact)
// Parts joined by >> narrow the match: each part is searched inside the previous matches, or is
// nth=N (0-based, negative from the end) or has-text="Foo"i (keeps matches containing the text).
// Modes: attached | visible | click | box | rect | element | clear | changed.
((sel, mode) => {
  const norm = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const box = (el) => el.getBoundingClientRect();
  const visible = (el) => {
    const r = box(el);
    return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden';
  };

  // Splits on >> outside quotes, so text="a >> b" stays one part.
  const split = (s) => {
    const parts = [];
    let cur = '';
    let quote = null;
    for (let i = 0; i < s.length; i++) {
      const c = s[i];
      if (quote) {
        cur += c;
        if (c === '\\') cur += s[++i] ?? '';
        else if (c === quote) quote = null;
      } else if (c === '"' || c === "'") {
        quote = c;
        cur += c;
      } else if (c === '>' && s[i + 1] === '>') {
        parts.push(cur.trim());
        cur = '';
        i++;
      } else {
        cur += c;
      }
    }
    parts.push(cur.trim());
    return parts;
  };

  // "Foo" is exact; "Foo"i and a bare Foo are case-insensitive substrings. `has` decides whether
  // anything inside an element can still match, `is` whether the element itself does.
  const matcher = (body) => {
    const q = /^("(?:[^"\\]|\\.)*")(i?)$/.exec(body);
    const want = norm(q ? JSON.parse(q[1]) : body);
    const exact = !!q && !q[2];
    const lw = want.toLowerCase();
    const has = (t) => (exact ? t.includes(want) : t.toLowerCase().includes(lw));
    return { has, is: (t) => (exact ? t === want : has(t)) };
  };

  const implicitRole = (el) => {
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
    (el.getAttribute('role') || '').trim().split(/\s+/)[0].toLowerCase() || implicitRole(el);
  const nameFromContent =
    /^(button|link|heading|cell|columnheader|option|listitem|tab|menuitem|checkbox|radio|switch|treeitem|row)$/;
  // Name from content (AccName 2F, compact): descendant text, alt text, aria-label and input
  // button values, skipping hidden descendants. <button><img alt="Save"></button> is "Save".
  const contentName = (node) => {
    if (node.nodeType === Node.TEXT_NODE) return node.textContent;
    if (node.nodeType !== Node.ELEMENT_NODE) return '';
    const st = getComputedStyle(node);
    if (node.getAttribute('aria-hidden') === 'true' || st.display === 'none' || st.visibility === 'hidden') return '';
    const label = norm(node.getAttribute('aria-label'));
    if (label) return label;
    if (node.tagName === 'IMG' || (node.tagName === 'INPUT' && node.type === 'image')) return node.getAttribute('alt') || '';
    if (node.tagName === 'INPUT' && /^(button|submit|reset)$/i.test(node.type)) return node.value;
    return [...node.childNodes].map(contentName).join(' ');
  };
  const labelledBy = (el) => {
    const ids = el.getAttribute('aria-labelledby');
    return ids ? norm(ids.split(/\s+/).map((id) => document.getElementById(id)?.textContent).join(' ')) : '';
  };
  // A compact accessible-name computation: aria-labelledby, aria-label, <label>, value of
  // input buttons, alt, then content for roles named by content, placeholder, title.
  const nameOf = (el) => {
    const byIds = labelledBy(el);
    if (byIds) return byIds;
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
      const t = norm([...el.childNodes].map(contentName).join(' '));
      if (t) return t;
    }
    return norm(el.getAttribute('placeholder')) || norm(el.getAttribute('title'));
  };
  // Hidden elements are not in the accessibility tree, so getByRole never matches them.
  const rendered = (el) =>
    el.getClientRects().length > 0 &&
    getComputedStyle(el).visibility !== 'hidden' &&
    !el.closest('[aria-hidden="true"]');

  // Each engine returns its matches inside `root` in document order; only the first when `first`.
  const pick = (els, keep, first) => (first ? [els.find(keep)].filter(Boolean) : els.filter(keep));
  const engines = {
    css: (body, root, first) =>
      first ? [root.querySelector(body)].filter(Boolean) : [...root.querySelectorAll(body)],
    // The deepest element whose text matches wins, like Playwright.
    text: (body, root, first) => {
      const m = matcher(body);
      const skip = /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|HEAD)$/;
      // Input buttons show their value, not text content; they are matched in the same walk so
      // the first match in DOM order wins.
      const buttons = 'input[type=button],input[type=submit],input[type=reset]';
      const out = [];
      // True when el or something inside it matched.
      const walk = (el) => {
        if (el.matches(buttons)) {
          if (!m.is(norm(el.value))) return false;
          out.push(el);
          return true;
        }
        const t = norm(el.textContent);
        if (!m.has(t) && !el.querySelector(buttons)) return false; // nothing below can match
        let below = false;
        for (const c of el.children) {
          if (skip.test(c.tagName)) continue;
          if (walk(c)) below = true;
          if (below && first) return true;
        }
        if (below || !m.is(t)) return below;
        out.push(el);
        return true;
      };
      const start = root === document ? document.body : root;
      if (start) walk(start);
      return out;
    },
    role: (body, root, first) => {
      const r = /^([\w-]+)(?:\[name=("(?:[^"\\]|\\.)*")(s?)\])?$/.exec(body);
      if (!r) throw new SyntaxError('expected role=button or role=button[name="Save"], got ' + body);
      const want = r[1].toLowerCase();
      const name = r[2] === undefined ? null : norm(JSON.parse(r[2]));
      const exact = r[3] === 's';
      const nameOk = (n) =>
        name === null || (exact ? n === name : n.toLowerCase().includes(name.toLowerCase()));
      return pick([...root.querySelectorAll('*')], (el) => roleOf(el) === want && rendered(el) && nameOk(nameOf(el)), first);
    },
    // <label> (for= or wrapping), aria-labelledby or aria-label, as in Playwright's getByLabel.
    label: (body, root, first) => {
      const m = matcher(body);
      const labels = (el) => [
        labelledBy(el),
        norm(el.getAttribute('aria-label')),
        ...[...(el.labels || [])].map((l) => norm(l.textContent)),
      ];
      return pick([...root.querySelectorAll('*')], (el) => labels(el).some((t) => t && m.is(t)), first);
    },
    placeholder: (body, root, first) => {
      const m = matcher(body);
      return pick([...root.querySelectorAll('[placeholder]')], (el) => m.is(norm(el.getAttribute('placeholder'))), first);
    },
    testid: (body, root, first) => {
      const id = body.startsWith('"') ? JSON.parse(body) : body;
      return pick([...root.querySelectorAll('[data-testid]')], (el) => el.getAttribute('data-testid') === id, first);
    },
  };
  const inOrder = (els) =>
    [...new Set(els)].sort((a, b) => (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1));
  // Throws SyntaxError on a malformed selector (caught below).
  const find = () => {
    const parts = split(sel);
    let found = null; // null until the first part searched the document
    parts.forEach((part, i) => {
      const m = /^(css|text|role|label|placeholder|testid|nth|has-text)=([\s\S]*)$/.exec(part);
      const [engine, body] = m ? [m[1], m[2]] : ['css', part];
      if ((engine === 'nth' || engine === 'has-text') && found === null) {
        throw new SyntaxError(engine + '= needs a selector before it');
      }
      if (engine === 'nth') {
        const n = Number(body);
        if (!Number.isInteger(n)) throw new SyntaxError('nth= expects an integer, got ' + body);
        found = [found[n < 0 ? found.length + n : n]].filter(Boolean);
      } else if (engine === 'has-text') {
        const t = matcher(body);
        found = found.filter((el) => t.has(norm(el.textContent)));
      } else {
        // Only the last part may stop at its first match: later parts need them all.
        const first = i === parts.length - 1;
        found = found === null
          ? engines[engine](body, document, first)
          : inOrder(found.flatMap((root) => engines[engine](body, root, first)));
      }
    });
    return found[0] || null;
  };

  // fill() steps act on the element the click just checked, not a fresh match: the page may
  // have replaced it, and a new match could be another element.
  if (mode === 'clear' || mode === 'changed') {
    const t = globalThis.__fluxwrightTarget;
    if (!t || !t.isConnected) return { ok: false, reason: 'element was removed after the click' };
    if (mode === 'changed') {
      t.dispatchEvent(new Event('input', { bubbles: true }));
      t.dispatchEvent(new Event('change', { bubbles: true }));
      return { ok: true };
    }
    const typable = t.tagName === 'TEXTAREA' ||
      (t.tagName === 'INPUT' && !/^(button|submit|reset|checkbox|radio|file|image|range|color|hidden)$/i.test(t.type));
    if (!typable && !t.isContentEditable) return { ok: false, reason: 'not an <input>, <textarea> or contenteditable element' };
    if (t.readOnly) return { ok: false, reason: 'element is read-only' };
    t.focus();
    if (t.isContentEditable) {
      // Select the old content so the typed text replaces it.
      const range = document.createRange();
      range.selectNodeContents(t);
      getSelection().removeAllRanges();
      getSelection().addRange(range);
    } else {
      t.value = '';
    }
    const active = t.getRootNode().activeElement;
    if (active !== t && !t.contains(active)) return { ok: false, reason: 'element did not take focus' };
    return { ok: true };
  }

  let el;
  try {
    el = find();
  } catch (e) {
    return { ok: false, fatal: true, reason: 'invalid selector: ' + e.message };
  }
  if (!el) return { ok: false, reason: 'no element matches selector' };
  if (mode === 'element') return el; // as a remote object, for locator.evaluate
  if (mode === 'attached') return { ok: true, text: el.textContent };
  if (mode === 'rect') {
    // Playwright's boundingBox: no scrolling, null when not visible.
    if (!visible(el)) return { ok: true, box: null };
    const r = box(el);
    return { ok: true, box: { x: r.x, y: r.y, width: r.width, height: r.height } };
  }
  if (!visible(el)) return { ok: false, reason: 'hidden' };
  if (mode === 'visible') return { ok: true, text: el.textContent };
  if (mode === 'box') {
    el.scrollIntoViewIfNeeded(true);
    const r = box(el);
    return { ok: true, x: r.x, y: r.y, width: r.width, height: r.height };
  }
  // click: enabled, scrolled into view, and the topmost element at its centre.
  // :disabled covers <fieldset disabled> ancestors; aria-disabled applies to descendants.
  if (el.matches(':disabled') || el.closest('[aria-disabled="true"]')) {
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
  globalThis.__fluxwrightTarget = el; // isolated world: invisible to page scripts
  return { ok: true, x, y };
})
