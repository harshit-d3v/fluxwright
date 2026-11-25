// Selector engine and element checks, evaluated in the page (or an iframe) on every poll.
// Called as ENGINE(selector, mode). Selectors, as in Playwright:
//   css (plain or css=...)   text=Foo or text="Foo"i (case-insensitive substring)   text="Foo" (exact)
//   role=button   role=button[name="Save"] (case-insensitive substring)   [name="Save"s] (exact)
//   label=... and placeholder=... (matched like text=)   testid="id" (data-testid, exact)
// Parts joined by >> narrow the match: each part is searched inside the previous matches, or is
// nth=N (0-based, negative from the end) or has-text="Foo"i (keeps matches containing the text).
// Modes: attached | visible | click | point | box | rect | rects | element | clear | changed | focus | blur | scroll | check_state | select_options | check_file_input | set_files.
((sel, mode, arg, refEl) => {
  const norm = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const box = (el) => el.getBoundingClientRect();
  const visible = (el) => {
    const r = box(el);
    return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden';
  };
  const ownsNode = (ancestor, node) => {
    for (let cur = node; cur; cur = cur.assignedSlot || cur.parentNode || cur.host) {
      if (cur === ancestor) return true;
    }
    return false;
  };
  // Collects `root`, its open shadowRoot, and open shadowRoots of descendants in tree order.
  const collectRoots = (root) => {
    const roots = [];
    const seen = new Set();
    const visit = (r) => {
      if (!r || typeof r.querySelectorAll !== 'function' || seen.has(r)) return;
      seen.add(r);
      roots.push(r);
      if (r.shadowRoot) visit(r.shadowRoot);
      for (const el of r.querySelectorAll('*')) {
        if (el.shadowRoot) visit(el.shadowRoot);
      }
    };
    visit(root);
    return roots;
  };
  const composedChildNodes = (el) => {
    if (el.shadowRoot) return [...el.shadowRoot.childNodes];
    if (el.tagName === 'SLOT' && typeof el.assignedNodes === 'function') {
      const assigned = el.assignedNodes({ flatten: true });
      return assigned.length ? assigned : [...el.childNodes];
    }
    return [...el.childNodes];
  };
  const composedPath = (node) => {
    const path = [];
    for (let cur = node; cur; cur = cur.assignedSlot || cur.parentNode || cur.host) {
      path.push(cur);
    }
    return path.reverse();
  };
  // Compares nodes across shadow boundaries and slots via their LCA in the composed tree.
  const compareComposedOrder = (a, b) => {
    if (a === b) return 0;
    const pa = composedPath(a);
    const pb = composedPath(b);
    let i = 0;
    while (i < pa.length && i < pb.length && pa[i] === pb[i]) i++;
    if (i === 0) {
      const pos = a.compareDocumentPosition(b);
      if (pos & Node.DOCUMENT_POSITION_FOLLOWING) return -1;
      if (pos & Node.DOCUMENT_POSITION_PRECEDING) return 1;
      return 0;
    }
    if (i === pa.length) return -1;
    if (i === pb.length) return 1;
    const lca = pa[i - 1];
    const ca = pa[i];
    const cb = pb[i];
    if (ca === lca.shadowRoot) return -1;
    if (cb === lca.shadowRoot) return 1;
    if (lca.tagName === 'SLOT' && typeof lca.assignedNodes === 'function') {
      const assigned = lca.assignedNodes();
      const ia = assigned.indexOf(ca);
      const ib = assigned.indexOf(cb);
      if (ia !== -1 && ib !== -1) return ia - ib;
      if (ia !== -1) return -1;
      if (ib !== -1) return 1;
    }
    const pos = ca.compareDocumentPosition(cb);
    if (pos & Node.DOCUMENT_POSITION_FOLLOWING) return -1;
    if (pos & Node.DOCUMENT_POSITION_PRECEDING) return 1;
    return 0;
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

  // "Foo" is exact; "Foo"i, bare Foo are case-insensitive substrings; re:"pat","flags" is RegExp.
  // `has` decides whether anything inside an element can still match, `is` whether the element itself does.
  const matcher = (body) => {
    if (body.startsWith('re:')) {
      const re = parseRe(body);
      return { has: (t) => re.test(t), is: (t) => re.test(t), prune: false };
    }
    const q = /^("(?:[^"\\]|\\.)*")(i?)$/.exec(body);
    const want = norm(q ? JSON.parse(q[1]) : body);
    const exact = !!q && !q[2];
    const lw = want.toLowerCase();
    const has = (t) => (exact ? t.includes(want) : t.toLowerCase().includes(lw));
    return { has, is: (t) => (exact ? t === want : has(t)), prune: true };
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
  const contentName = (node, includeHidden) => {
    if (!node) return '';
    if (node.nodeType === Node.TEXT_NODE) return node.textContent;
    if (node.nodeType !== Node.ELEMENT_NODE || /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/.test(node.tagName)) return '';
    const st = getComputedStyle(node);
    if (!includeHidden && (node.getAttribute('aria-hidden') === 'true' || st.display === 'none' || st.visibility === 'hidden')) return '';
    const label = norm(node.getAttribute('aria-label'));
    if (label) return label;
    if (node.tagName === 'IMG' || (node.tagName === 'INPUT' && node.type === 'image')) return node.getAttribute('alt') || '';
    if (node.tagName === 'INPUT' && /^(button|submit|reset)$/i.test(node.type)) return node.value;
    return composedChildNodes(node).map((c) => contentName(c, includeHidden)).join(' ');
  };
  const labelledBy = (el) => {
    const ids = el.getAttribute('aria-labelledby');
    if (!ids) return '';
    const root = el.getRootNode();
    const lookup = (id) =>
      (root && typeof root.getElementById === 'function' && root.getElementById(id)) ||
      document.getElementById(id);
    return norm(
      ids
        .split(/\s+/)
        .filter(Boolean)
        .map((id) => {
          const target = lookup(id);
          return target ? contentName(target, true) || target.textContent : '';
        })
        .join(' '),
    );
  };
  // A compact accessible-name computation: aria-labelledby, aria-label, <label>, value of
  // input buttons, alt, then content for roles named by content, placeholder, title.
  const nameOf = (el, includeHidden) => {
    const byIds = labelledBy(el);
    if (byIds) return byIds;
    const label = norm(el.getAttribute('aria-label'));
    if (label) return label;
    if (el.labels && el.labels.length) return norm([...el.labels].map((l) => shownText(l)).join(' '));
    if (el.tagName === 'INPUT' && /^(button|submit|reset)$/i.test(el.type)) {
      return norm(el.value) || { submit: 'Submit', reset: 'Reset' }[el.type] || '';
    }
    if (el.tagName === 'IMG' || el.tagName === 'AREA' || (el.tagName === 'INPUT' && el.type === 'image')) {
      const alt = norm(el.getAttribute('alt'));
      if (alt) return alt;
    }
    if (nameFromContent.test(roleOf(el) || '')) {
      const t = norm(composedChildNodes(el).map((c) => contentName(c, includeHidden)).join(' '));
      if (t) return t;
    }
    return norm(el.getAttribute('placeholder')) || norm(el.getAttribute('title'));
  };
  const isAriaHidden = (el) => {
    for (let cur = el; cur; cur = cur.assignedSlot || cur.parentNode || cur.host) {
      if (cur.nodeType === 1 && cur.getAttribute('aria-hidden') === 'true') return true;
    }
    return false;
  };
  // Hidden elements are not in the accessibility tree, so getByRole never matches them.
  const rendered = (el) =>
    el.getClientRects().length > 0 &&
    getComputedStyle(el).visibility !== 'hidden' &&
    !isAriaHidden(el);

  // Input buttons show their value, not text content.
  const buttons = 'input[type=button],input[type=submit],input[type=reset]';
  // An element's text as the page shows it: button values included, scripts and styles left out.
  const shownText = (node) => {
    if (node.nodeType === Node.TEXT_NODE) return node.textContent;
    if (node.nodeType !== Node.ELEMENT_NODE || /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/.test(node.tagName)) return '';
    if (node.matches(buttons)) return ' ' + node.value + ' ';
    return composedChildNodes(node).map(shownText).join('');
  };

  const queryAllDeep = (root, selector) => {
    const roots = collectRoots(root);
    if (roots.length === 1) return [...roots[0].querySelectorAll(selector)];
    return inOrder(roots.flatMap((r) => [...r.querySelectorAll(selector)]));
  };
  // Splits a CSS selector list on top-level commas, and each branch on top-level combinators
  // (' ', '>', '+', '~') outside quotes, escapes, brackets and parentheses.
  const parseCssSelector = (sel) => {
    const branches = [];
    let steps = [];
    let cur = '';
    let pendingComb = null;
    let quote = null;
    let paren = 0;
    let bracket = 0;
    let branchStart = 0;
    const flushPart = (nextComb) => {
      if (cur) {
        steps.push({ comb: pendingComb, sel: cur });
        cur = '';
        pendingComb = nextComb;
      } else if (nextComb && nextComb !== ' ' && pendingComb === ' ') {
        pendingComb = nextComb;
      }
    };
    const flushBranch = (end) => {
      flushPart(null);
      if (steps.length) branches.push({ raw: sel.slice(branchStart, end).trim(), steps });
      steps = [];
      pendingComb = null;
      branchStart = end + 1;
    };
    for (let i = 0; i < sel.length; i++) {
      const c = sel[i];
      if (quote) {
        cur += c;
        if (c === '\\') cur += sel[++i] ?? '';
        else if (c === quote) quote = null;
      } else if (c === '\\') {
        cur += c + (sel[++i] ?? '');
      } else if (c === '"' || c === "'") {
        quote = c;
        cur += c;
      } else if (c === '(') {
        paren++;
        cur += c;
      } else if (c === ')') {
        if (paren > 0) paren--;
        cur += c;
      } else if (c === '[') {
        bracket++;
        cur += c;
      } else if (c === ']') {
        if (bracket > 0) bracket--;
        cur += c;
      } else if (paren === 0 && bracket === 0) {
        if (c === ',') {
          flushBranch(i);
        } else if (c === '>' || c === '+' || c === '~') {
          flushPart(c);
        } else if (/\s/.test(c)) {
          if (cur) flushPart(' ');
        } else {
          cur += c;
        }
      } else {
        cur += c;
      }
    }
    flushBranch(sel.length);
    return branches;
  };

  // Each engine returns its matches inside `root` in composed document order; only the first when `first`.
  const pick = (els, keep, first) => (first ? [els.find(keep)].filter(Boolean) : els.filter(keep));
  const engines = {
    'css:light': (body, root, first) =>
      first ? [root.querySelector(body)].filter(Boolean) : [...root.querySelectorAll(body)],
    css: (body, root, first) => {
      const roots = collectRoots(root);
      if (roots.length === 1) {
        return first ? [root.querySelector(body)].filter(Boolean) : [...root.querySelectorAll(body)];
      }
      root.querySelectorAll(body); // validate CSS syntax before stepping
      const out = [];
      for (const { raw, steps } of parseCssSelector(body)) {
        for (const r of roots) out.push(...r.querySelectorAll(raw));
        if (steps.length > 1) {
          let cur = steps[0].sel === ':scope'
            ? (root === document ? [document.documentElement].filter(Boolean) : [root])
            : roots.flatMap((r) => [...r.querySelectorAll(steps[0].sel)]);
          for (let i = 1; i < steps.length && cur.length; i++) {
            const { comb, sel } = steps[i];
            const next = [];
            const seen = new Set();
            const add = (el) => {
              if (!seen.has(el)) {
                seen.add(el);
                next.push(el);
              }
            };
            for (const el of cur) {
              if (comb === ' ') {
                for (const r of collectRoots(el)) {
                  for (const m of r.querySelectorAll(sel)) add(m);
                }
              } else if (comb === '>') {
                for (const c of el.children) if (c.matches(sel)) add(c);
                if (el.shadowRoot) {
                  for (const c of el.shadowRoot.children) if (c.matches(sel)) add(c);
                }
              } else if (comb === '+') {
                const sib = el.nextElementSibling;
                if (sib && sib.matches(sel) && (root === document || ownsNode(root, sib))) add(sib);
              } else if (comb === '~') {
                for (let sib = el.nextElementSibling; sib; sib = sib.nextElementSibling) {
                  if (sib.matches(sel) && (root === document || ownsNode(root, sib))) add(sib);
                }
              }
            }
            cur = next;
          }
          out.push(...cur);
        }
      }
      const sorted = inOrder(out);
      return first ? sorted.slice(0, 1) : sorted;
    },
    // The deepest element whose text matches wins, like Playwright.
    text: (body, root, first) => {
      const m = matcher(body);
      const skip = /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|HEAD)$/;
      // Input buttons are matched in the same walk, so the first match in composed order wins.
      const out = [];
      const start = root === document ? document.body : root;
      if (!start) return out;
      const roots = collectRoots(start);
      const hasShadowSubtree = (el) => {
        if (el.shadowRoot || el.tagName === 'SLOT' || el.querySelector('slot')) return true;
        for (const r of roots) {
          if (r.host && el.contains(r.host)) return true;
        }
        return false;
      };
      // True when el or something inside it matched.
      const walk = (el) => {
        if (el.matches(buttons)) {
          if (!m.is(norm(el.value))) return false;
          out.push(el);
          return true;
        }
        // textContent includes <script> text: fine to rule a subtree out when no shadow/slot is below.
        if (m.prune && !hasShadowSubtree(el) && !m.has(norm(el.textContent)) && !el.querySelector(buttons)) return false;
        let below = false;
        for (const c of composedChildNodes(el)) {
          if (c.nodeType !== Node.ELEMENT_NODE || skip.test(c.tagName)) continue;
          if (walk(c)) below = true;
          if (below && first) return true;
        }
        if (below || el.tagName === 'SLOT' || !m.is(norm(shownText(el)))) return below;
        out.push(el);
        return true;
      };
      walk(start);
      return out;
    },
    role: (body, root, first) => {
      const m = /^([\w-]+)([\s\S]*)$/.exec(body);
      if (!m) throw new SyntaxError('expected role=button, got ' + body);
      const want = m[1].toLowerCase();
      let rest = m[2];
      let name = null;
      let exact = false;
      let nameRe = null;
      let includeHidden = false;
      const checks = [];
      const attrRe = /^\[([\w-]+)=("(?:[^"\\]|\\.)*"s?|re:"(?:[^"\\]|\\.)*","(?:[^"\\]|\\.)*"|true|false|\d+)\]/;
      while (rest.length) {
        const a = attrRe.exec(rest);
        if (!a) throw new SyntaxError('invalid role attribute in ' + body);
        rest = rest.slice(a[0].length);
        const [, k, v] = a;
        if (k === 'name') {
          if (v.startsWith('re:')) {
            nameRe = parseRe(v);
          } else {
            exact = v.endsWith('s');
            name = norm(JSON.parse(exact ? v.slice(0, -1) : v));
          }
        } else if (k === 'include-hidden') {
          includeHidden = v === 'true';
        } else if (k === 'level') {
          const wantLevel = Number(v);
          checks.push((el) => levelOf(el) === wantLevel);
        } else if (k === 'disabled') {
          const wantDisabled = v === 'true';
          checks.push((el) => isDisabledEl(el) === wantDisabled);
        } else if (k === 'checked') {
          const wantChecked = v === 'true';
          checks.push((el) => isCheckedEl(el) === wantChecked);
        } else if (k === 'selected') {
          const wantSelected = v === 'true';
          checks.push((el) => isSelectedEl(el) === wantSelected);
        } else if (k === 'expanded') {
          const wantExpanded = v === 'true';
          checks.push((el) => isExpandedEl(el) === wantExpanded);
        } else if (k === 'pressed') {
          const wantPressed = v === 'true';
          checks.push((el) => isPressedEl(el) === wantPressed);
        } else {
          throw new SyntaxError('unknown role attribute ' + k);
        }
      }
      const nameOk = (n) =>
        nameRe
          ? nameRe.test(n)
          : name === null || (exact ? n === name : n.toLowerCase().includes(name.toLowerCase()));
      return pick(
        queryAllDeep(root, '*'),
        (el) => roleOf(el) === want && (includeHidden || rendered(el)) && checks.every((fn) => fn(el)) && nameOk(nameOf(el, includeHidden)),
        first,
      );
    },
    xpath: (body, root, first) => {
      const expr = root !== document && body.startsWith('/') ? '.' + body : body;
      let snap;
      try {
        snap = document.evaluate(expr, root, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
      } catch (e) {
        throw new SyntaxError('invalid xpath ' + body + ': ' + e.message);
      }
      const out = [];
      for (let i = 0; i < snap.snapshotLength; i++) {
        const node = snap.snapshotItem(i);
        if (node && node.nodeType === Node.ELEMENT_NODE) {
          out.push(node);
          if (first) break;
        }
      }
      return out;
    },
    // <label> (for= or wrapping), aria-labelledby or aria-label, as in Playwright's getByLabel.
    label: (body, root, first) => {
      const m = matcher(body);
      const labels = (el) => [
        labelledBy(el),
        norm(el.getAttribute('aria-label')),
        ...[...(el.labels || [])].map((l) => norm(shownText(l))),
      ];
      return pick(queryAllDeep(root, '*'), (el) => labels(el).some((t) => t && m.is(t)), first);
    },
    placeholder: (body, root, first) => {
      const m = matcher(body);
      return pick(queryAllDeep(root, '[placeholder]'), (el) => m.is(norm(el.getAttribute('placeholder'))), first);
    },
    alt: (body, root, first) => {
      const m = matcher(body);
      return pick(
        queryAllDeep(root, 'img[alt],area[alt],input[type=image][alt]'),
        (el) => m.is(norm(el.getAttribute('alt'))),
        first,
      );
    },
    title: (body, root, first) => {
      const m = matcher(body);
      return pick(queryAllDeep(root, '[title]'), (el) => m.is(norm(el.getAttribute('title'))), first);
    },
    testid: (body, root, first) => {
      const id = body.startsWith('"') ? JSON.parse(body) : body;
      return pick(queryAllDeep(root, '[data-testid]'), (el) => el.getAttribute('data-testid') === id, first);
    },
    ref: (_body, root) =>
      refEl && refEl.nodeType === 1 && refEl.isConnected && (root === document || ownsNode(root, refEl))
        ? [refEl]
        : [],
  };
  function parseRe(body) {
    const m = /^re:("(?:[^"\\]|\\.)*"),("(?:[^"\\]|\\.)*")$/.exec(body);
    if (!m) throw new SyntaxError('invalid regex selector: ' + body);
    return new RegExp(JSON.parse(m[1]), JSON.parse(m[2]).replace(/[gy]/g, ''));
  }
  function ariaBool(el, attr) {
    const v = el.getAttribute(attr);
    return v === null ? null : v.toLowerCase();
  }
  function isDisabledEl(el) {
    if (el.matches(':disabled')) return true;
    for (let cur = el; cur; cur = cur.assignedSlot || cur.parentNode || cur.host) {
      if (cur.nodeType !== 1) continue;
      const v = cur.getAttribute('aria-disabled');
      if (v !== null) return v.toLowerCase() === 'true';
    }
    return false;
  }
  function isCheckedEl(el) {
    if (el.tagName === 'INPUT' && (el.type === 'checkbox' || el.type === 'radio')) {
      return el.indeterminate ? 'mixed' : !!el.checked;
    }
    const v = ariaBool(el, 'aria-checked');
    if (v === 'true') return true;
    if (v === 'false') return false;
    if (v === 'mixed') return 'mixed';
    return /^(checkbox|radio|switch|menuitemcheckbox|menuitemradio)$/.test(roleOf(el)) ? false : null;
  }
  function isSelectedEl(el) {
    if (el.tagName === 'OPTION') return !!el.selected;
    const v = ariaBool(el, 'aria-selected');
    if (v === 'true') return true;
    if (v === 'false') return false;
    return /^(gridcell|option|row|tab)$/.test(roleOf(el)) ? false : null;
  }
  function isExpandedEl(el) {
    const v = ariaBool(el, 'aria-expanded');
    if (v === 'true') return true;
    if (v === 'false') return false;
    return el.tagName === 'DETAILS' ? !!el.open : null;
  }
  function isPressedEl(el) {
    const v = ariaBool(el, 'aria-pressed');
    if (v === 'true') return true;
    if (v === 'false') return false;
    if (v === 'mixed') return 'mixed';
    return null;
  }
  function levelOf(el) {
    const v = el.getAttribute('aria-level');
    if (v !== null) {
      const n = Number(v);
      if (Number.isInteger(n) && n >= 1) return n;
    }
    const hm = /^H([1-6])$/.exec(el.tagName);
    return hm ? Number(hm[1]) : null;
  }
  const describeEl = (el) => {
    const html = el.outerHTML || ('<' + el.tagName.toLowerCase() + '>');
    const idx = html.indexOf('>');
    const head = idx >= 0 ? html.slice(0, idx + 1) : html;
    return head.length > 80 ? head.slice(0, 77) + '...' : head;
  };
  const strictViolation = (s, list) =>
    `strict mode violation: ${s} resolved to ${list.length} elements:\n` +
    list.slice(0, 10).map((e, i) => `    ${i + 1}) ${describeEl(e)}`).join('\n');
  const inOrder = (els) => [...new Set(els)].sort(compareComposedOrder);
  // Throws SyntaxError on a malformed selector (caught below).
  const findAll = (selStr, docRoot, limitOne) => {
    const parts = split(selStr);
    let found = null; // null until the first part searched docRoot
    parts.forEach((part, i) => {
      const m = /^(css:light|css|xpath|text|role|label|placeholder|alt|title|testid|ref|nth|has-text|has-not-text|has|has-not|and|or)=([\s\S]*)$/.exec(part);
      const [engine, body] = m
        ? [m[1], m[2]]
        : /^@e\d+(?:@r\d+)?$/.test(part)
          ? ['ref', part.slice(1)]
          : part.startsWith('//') || part.startsWith('..')
            ? ['xpath', part]
            : ['css', part];
      if ((engine === 'nth' || engine === 'has-text' || engine === 'has-not-text' || engine === 'has' || engine === 'has-not' || engine === 'and' || engine === 'or') && found === null) {
        throw new SyntaxError(engine + '= needs a selector before it');
      }
      if (engine === 'nth') {
        const n = Number(body);
        if (!Number.isInteger(n)) throw new SyntaxError('nth= expects an integer, got ' + body);
        found = [found[n < 0 ? found.length + n : n]].filter(Boolean);
      } else if (engine === 'has-text' || engine === 'has-not-text') {
        const t = matcher(body);
        const want = engine === 'has-text';
        found = found.filter((el) => t.has(norm(shownText(el))) === want);
      } else if (engine === 'has' || engine === 'has-not') {
        const sub = JSON.parse(body);
        const want = engine === 'has';
        found = found.filter((el) => (findAll(sub, el, true).length > 0) === want);
      } else if (engine === 'and') {
        const sub = JSON.parse(body);
        const other = new Set(findAll(sub, docRoot, false));
        found = found.filter((el) => other.has(el));
      } else if (engine === 'or') {
        const sub = JSON.parse(body);
        found = inOrder([...found, ...findAll(sub, docRoot, false)]);
      } else {
        // Only the last part may stop at its first match when strict/bulk don't need them all.
        const first = limitOne && i === parts.length - 1;
        found = found === null
          ? engines[engine](body, docRoot, first)
          : inOrder(found.flatMap((root) => engines[engine](body, root, first)));
      }
    });
    return found || [];
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

  let matches;
  const strict = !!(arg && arg.strict);
  const bulk =
    mode === 'count' ||
    mode === 'all_text_contents' ||
    mode === 'all_inner_texts' ||
    mode === 'rects';
  try {
    matches = [...new Set(findAll(sel, document, !strict && !bulk))];
  } catch (e) {
    return { ok: false, fatal: true, reason: 'invalid selector: ' + e.message };
  }
  if (mode === 'count') return { ok: true, count: matches.length };
  if (mode === 'all_text_contents') return { ok: true, texts: matches.map((e) => e.textContent ?? '') };
  if (mode === 'all_inner_texts') {
    return { ok: true, texts: matches.map((e) => (e instanceof HTMLElement ? e.innerText : e.textContent) ?? '') };
  }
  if (mode === 'rects') {
    const rects = [];
    for (const el of matches) {
      if (!el || !el.isConnected || !visible(el)) continue;
      const r = box(el);
      if (r.width > 0 && r.height > 0) {
        rects.push({ x: r.x, y: r.y, width: r.width, height: r.height });
      }
    }
    return { ok: true, rects };
  }
  if (strict && matches.length > 1) {
    return { ok: false, fatal: true, reason: strictViolation(sel, matches) };
  }
  if (mode === 'is_visible') {
    return { ok: true, visible: matches.length === 1 && matches[0].isConnected && visible(matches[0]) };
  }
  const el = matches[0] || null;
  if (!el) return { ok: false, reason: 'no element matches selector' };
  if (mode === 'element') return el; // as a remote object, for locator.evaluate
  if (mode === 'attached') return { ok: true, text: el.textContent };
  if (mode === 'inner_text') {
    return { ok: true, text: (el instanceof HTMLElement ? el.innerText : el.textContent) ?? '' };
  }
  if (mode === 'inner_html') return { ok: true, html: el.innerHTML };
  if (mode === 'input_value') {
    if (!/^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName)) {
      return { ok: false, fatal: true, reason: 'element is not an <input>, <textarea> or <select> element' };
    }
    return { ok: true, value: el.value };
  }
  if (mode === 'get_attribute') {
    const name = arg && arg.name;
    return { ok: true, value: el.hasAttribute(name) ? el.getAttribute(name) : null };
  }
  if (mode === 'is_enabled') return { ok: true, enabled: !isDisabledEl(el) };
  if (mode === 'is_checked') {
    const v = isCheckedEl(el);
    if (v === null) {
      return { ok: false, fatal: true, reason: 'element is not a checkbox, radio button or switch' };
    }
    return { ok: true, checked: v === true };
  }
  if (mode === 'is_editable') {
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
      return {
        ok: true,
        editable: !el.readOnly && !isDisabledEl(el) && ariaBool(el, 'aria-readonly') !== 'true',
      };
    }
    if (el.tagName === 'SELECT') {
      return { ok: true, editable: !isDisabledEl(el) && ariaBool(el, 'aria-readonly') !== 'true' };
    }
    if (el.isContentEditable) {
      return { ok: true, editable: !isDisabledEl(el) && ariaBool(el, 'aria-readonly') !== 'true' };
    }
    const r = roleOf(el);
    if (/^(checkbox|combobox|gridcell|listbox|menuitemcheckbox|menuitemradio|radiogroup|searchbox|slider|spinbutton|switch|textbox)$/.test(r)) {
      return { ok: true, editable: !isDisabledEl(el) && ariaBool(el, 'aria-readonly') !== 'true' };
    }
    return {
      ok: false,
      fatal: true,
      reason: 'element is not an <input>, <textarea>, <select> or [contenteditable]',
    };
  }
  if (mode === 'focus') {
    el.focus();
    return { ok: true };
  }
  if (mode === 'blur') {
    el.blur();
    return { ok: true };
  }
  if (mode === 'scroll') {
    el.scrollIntoViewIfNeeded(true);
    return { ok: true };
  }
  if (mode === 'check_state') {
    const opts = arg || {};
    const isNative = el.tagName === 'INPUT' && /^(checkbox|radio)$/i.test(el.type);
    const role = roleOf(el);
    if (!isNative && !['checkbox', 'radio', 'switch'].includes(role)) {
      return { ok: false, fatal: true, reason: 'not a checkbox, radio or switch element' };
    }
    if (
      opts.want === false &&
      ((el.tagName === 'INPUT' && el.type.toLowerCase() === 'radio') || role === 'radio')
    ) {
      return { ok: false, fatal: true, reason: 'cannot uncheck a radio button' };
    }
    const checked = isNative ? !!el.checked : el.getAttribute('aria-checked') === 'true';
    return { ok: true, checked };
  }
  if (mode === 'select_options') {
    const opts = arg || {};
    if (el.tagName !== 'SELECT') {
      return { ok: false, fatal: true, reason: 'element is not a <select>' };
    }
    if (!opts.force) {
      if (!visible(el)) return { ok: false, reason: 'hidden' };
      if (isDisabledEl(el)) {
        return { ok: false, reason: 'disabled' };
      }
    }
    const options = [...el.options];
    const isDisabledOpt = (o) =>
      o.disabled || (o.parentElement && o.parentElement.tagName === 'OPTGROUP' && o.parentElement.disabled);
    const picked = [];
    for (const item of opts.items || []) {
      let match = null;
      if (typeof item.index === 'number') {
        const cand = options[item.index];
        if (cand && !isDisabledOpt(cand)) match = cand;
      } else if (typeof item.value === 'string') {
        match = options.find((o) => !isDisabledOpt(o) && o.value === item.value) || null;
      } else if (typeof item.label === 'string') {
        const want = norm(item.label);
        match =
          options.find((o) => !isDisabledOpt(o) && (norm(o.label) === want || norm(o.textContent) === want)) ||
          null;
      }
      if (!match) return { ok: false, reason: 'did not find option' };
      picked.push(match);
    }
    if (!el.multiple) {
      for (const o of options) o.selected = o === picked[0];
    } else {
      const set = new Set(picked);
      for (const o of options) o.selected = set.has(o);
    }
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return { ok: true, values: [...el.selectedOptions].map((o) => o.value) };
  }
  if (mode === 'check_file_input' || mode === 'set_files') {
    if (el.tagName !== 'INPUT' || el.type.toLowerCase() !== 'file') {
      return { ok: false, fatal: true, reason: 'element is not an <input type="file">' };
    }
    const files = (arg && arg.files) || [];
    const count = arg && typeof arg.count === 'number' ? arg.count : files.length;
    if (count > 1 && !el.multiple) {
      return { ok: false, fatal: true, reason: 'non-multiple file input can only accept single file' };
    }
    if (mode === 'check_file_input') return { ok: true };
    const dt = new DataTransfer();
    for (const f of files) {
      const bin = atob(f.base64);
      const bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      dt.items.add(new File([bytes], f.name, { type: f.mimeType || '' }));
    }
    el.files = dt.files;
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return { ok: true };
  }
  if (mode === 'rect') {
    // Playwright's boundingBox: no scrolling, null when not visible.
    if (!visible(el)) return { ok: true, box: null };
    const r = box(el);
    return { ok: true, box: { x: r.x, y: r.y, width: r.width, height: r.height } };
  }
  if (mode === 'point' || mode === 'click') {
    const opts = mode === 'click' ? { requireEnabled: true } : arg || {};
    if (!opts.force) {
      if (!visible(el)) return { ok: false, reason: 'hidden' };
      if (opts.requireEnabled && isDisabledEl(el)) {
        return { ok: false, reason: 'disabled' };
      }
    }
    el.scrollIntoViewIfNeeded(true);
    const r = box(el);
    if (r.width <= 0 || r.height <= 0) return { ok: false, reason: 'hidden' };
    const x = opts.position ? r.x + opts.position.x : r.x + r.width / 2;
    const y = opts.position ? r.y + opts.position.y : r.y + r.height / 2;
    if (!opts.force) {
      let hit = document.elementFromPoint(x, y);
      while (hit && hit !== el && hit.shadowRoot) {
        let inner = hit.shadowRoot.elementFromPoint(x, y);
        // Chromium retargets slotted text nodes back to an outer host in elementFromPoint;
        // elementsFromPoint exposes the shadow element wrapping the <slot>.
        if ((!inner || inner === hit || !ownsNode(hit, inner)) && typeof hit.shadowRoot.elementsFromPoint === 'function') {
          const stack = hit.shadowRoot.elementsFromPoint(x, y);
          const hostIdx = stack.indexOf(hit);
          const above = hostIdx >= 0 ? stack.slice(0, hostIdx) : stack;
          inner = above.find((e) => e !== hit && ownsNode(hit, e)) || null;
        }
        if (!inner || inner === hit || !ownsNode(hit, inner)) break;
        hit = inner;
      }
      if (!hit) return { ok: false, reason: 'centre is outside the viewport' };
      const hitLabel = hit.closest('label');
      const owns = hit === el || ownsNode(el, hit) || (hitLabel && hitLabel.control === el);
      if (!owns) {
        return { ok: false, reason: 'obscured by <' + hit.tagName.toLowerCase() + (hit.id ? '#' + hit.id : '') + '>' };
      }
    }
    globalThis.__fluxwrightTarget = el; // isolated world: invisible to page scripts
    return { ok: true, x, y };
  }
  if (!visible(el)) return { ok: false, reason: 'hidden' };
  if (mode === 'visible') return { ok: true, text: el.textContent };
  if (mode === 'box') {
    el.scrollIntoViewIfNeeded(true);
    const r = box(el);
    return { ok: true, x: r.x, y: r.y, width: r.width, height: r.height };
  }
})
