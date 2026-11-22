((opts, rootEl) => {
  const options = Object.assign({ mainContentOnly: false, includeLinks: true, includeImages: false }, opts || {});
  const SKIP_TAGS = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG', 'CANVAS', 'IFRAME', 'HEAD', 'OBJECT', 'EMBED']);

  const pickRoot = () => {
    if (rootEl && rootEl.nodeType === 1) return rootEl;
    if (options.mainContentOnly) {
      return document.querySelector('main, article, [role="main"]') || document.body || document.documentElement;
    }
    return document.body || document.documentElement;
  };

  const isHidden = (el) => {
    if (!el || el.nodeType !== 1) return false;
    if (el.hidden || el.getAttribute('aria-hidden') === 'true') return true;
    const st = getComputedStyle(el);
    return st.display === 'none' || st.visibility === 'hidden';
  };

  const cleanInline = (s) => (s || '').replace(/\s+/g, ' ').trim();

  const serializeChildren = (node, listDepth, inPre) =>
    Array.from(node.childNodes).map((c) => serialize(c, listDepth, inPre)).join('');

  const serializeList = (listEl, depth) => {
    const ordered = listEl.tagName === 'OL';
    const indent = '  '.repeat(depth);
    let idx = Number(listEl.getAttribute('start') || '1');
    if (!Number.isFinite(idx)) idx = 1;
    const lines = [];
    for (const child of listEl.children) {
      if (child.tagName !== 'LI' || isHidden(child)) continue;
      const prefix = ordered ? `${idx++}. ` : '- ';
      const parts = [];
      const subLists = [];
      for (const n of child.childNodes) {
        if (n.nodeType === 1 && (n.tagName === 'UL' || n.tagName === 'OL')) {
          subLists.push(serializeList(n, depth + 1));
        } else {
          parts.push(serialize(n, depth + 1, false));
        }
      }
      const line = parts.join('').replace(/\n+/g, ' ').trim();
      if (line) lines.push(`${indent}${prefix}${line}`);
      for (const sub of subLists) {
        if (sub) lines.push(sub);
      }
    }
    return lines.length ? `\n\n${lines.join('\n')}\n\n` : '';
  };

  const serializeTable = (table) => {
    const rows = Array.from(table.querySelectorAll('tr')).filter((tr) => !isHidden(tr));
    if (!rows.length) return '';
    const matrix = rows.map((tr) =>
      Array.from(tr.children)
        .filter((cell) => (cell.tagName === 'TH' || cell.tagName === 'TD') && !isHidden(cell))
        .map((cell) => cleanInline(serializeChildren(cell, 0, false)).replace(/\\/g, '\\\\').replace(/\|/g, '\\|'))
    ).filter((r) => r.length > 0);
    if (!matrix.length) return '';
    const cols = Math.max(...matrix.map((r) => r.length));
    const pad = (r) => {
      const copy = r.slice();
      while (copy.length < cols) copy.push('');
      return `| ${copy.join(' | ')} |`;
    };
    const out = [pad(matrix[0]), `| ${Array(cols).fill('---').join(' | ')} |`];
    for (let i = 1; i < matrix.length; i++) out.push(pad(matrix[i]));
    return `\n\n${out.join('\n')}\n\n`;
  };

  const serialize = (node, listDepth, inPre) => {
    if (!node) return '';
    if (node.nodeType === Node.TEXT_NODE) {
      const raw = node.textContent || '';
      if (inPre) return raw;
      if (!raw.trim()) return raw.length ? ' ' : '';
      return raw.replace(/\s+/g, ' ');
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return '';
    const tag = node.tagName;
    if (SKIP_TAGS.has(tag) || isHidden(node)) return '';

    if (tag === 'BR') return '\n';
    if (tag === 'HR') return '\n\n---\n\n';
    if (/^H([1-6])$/.test(tag)) {
      const lvl = Number(tag.slice(1));
      const text = cleanInline(serializeChildren(node, listDepth, false));
      return text ? `\n\n${'#'.repeat(lvl)} ${text}\n\n` : '';
    }
    if (tag === 'PRE') {
      const codeEl = node.querySelector('code');
      const raw = ((codeEl || node).textContent || '').replace(/^\n+|\n+$/g, '');
      return raw ? `\n\n\`\`\`\n${raw}\n\`\`\`\n\n` : '';
    }
    if (tag === 'CODE') {
      const text = (node.textContent || '').trim();
      return text ? `\`${text}\`` : '';
    }
    if (tag === 'STRONG' || tag === 'B') {
      const text = cleanInline(serializeChildren(node, listDepth, inPre));
      return text ? `**${text}**` : '';
    }
    if (tag === 'EM' || tag === 'I') {
      const text = cleanInline(serializeChildren(node, listDepth, inPre));
      return text ? `*${text}*` : '';
    }
    if (tag === 'A') {
      const text = cleanInline(serializeChildren(node, listDepth, inPre));
      const rawHref = (node.getAttribute('href') || '').trim();
      if (!options.includeLinks || !rawHref || /^javascript:/i.test(rawHref)) return text;
      const href = node.href || rawHref;
      return text ? `[${text}](${href})` : href;
    }
    if (tag === 'IMG') {
      if (!options.includeImages) return '';
      const src = node.currentSrc || node.src || node.getAttribute('src') || '';
      const alt = cleanInline(node.getAttribute('alt') || '');
      if (!src && !alt) return '';
      return `![${alt}](${src})`;
    }
    if (tag === 'UL' || tag === 'OL') {
      return serializeList(node, listDepth);
    }
    if (tag === 'TABLE') {
      return serializeTable(node);
    }
    if (tag === 'BLOCKQUOTE') {
      const body = serializeChildren(node, listDepth, false).trim();
      if (!body) return '';
      const quoted = body.split('\n').map((l) => `> ${l.trim()}`).join('\n');
      return `\n\n${quoted}\n\n`;
    }

    const inner = serializeChildren(node, listDepth, inPre);
    if (/^(P|DIV|SECTION|ARTICLE|MAIN|HEADER|FOOTER|NAV|ASIDE|FIGCAPTION|FIGURE)$/.test(tag)) {
      return inner.trim() ? `\n\n${inner.trim()}\n\n` : '';
    }
    return inner;
  };

  const root = pickRoot();
  if (!root) return '';
  return serialize(root, 0, false)
    .replace(/\r\n/g, '\n')
    .replace(/[ \t]+\n/g, '\n')
    .replace(/\n{3,}/g, '\n\n')
    .trim();
})
