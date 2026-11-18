// What the factory writes to the owner is Slack mrkdwn, or Markdown from an
// agent. This turns either into a little HTML. Everything is escaped first,
// and only the marks made here are added back, so nothing in the text can
// become markup of its own.

const escape = (text: string) =>
  text.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c] as string);

const emoji: Record<string, string> = {
  rocket: '🚀', question: '❓', hourglass_flowing_sand: '⏳', checkered_flag: '🏁', octagonal_sign: '🛑',
  x: '❌', white_check_mark: '✅', warning: '⚠️', bulb: '💡', mag: '🔍', red_circle: '🔴', tada: '🎉',
  wrench: '🔧', memo: '📝', inbox_tray: '📥', construction: '🚧', ship: '🚢', eyes: '👀', robot_face: '🤖',
};

function inline(text: string): string {
  // Code first, so that nothing inside it is read as a mark.
  const codes: string[] = [];
  let out = text.replace(/`([^`\n]+)`/g, (_, code) => {
    codes.push(code);
    return `\u0000${codes.length - 1}\u0000`;
  });
  out = escape(out);
  // Slack links: <https://x|label> arrive escaped as &lt;…&gt;.
  out = out.replace(/&lt;(https?:\/\/[^|\s&]+(?:&amp;[^|\s&]+)*)(?:\|([^&]+?))?&gt;/g, (_, url, label) =>
    `<a href="${url}" target="_blank" rel="noopener noreferrer">${label || url}</a>`);
  // A mention of a person: the owner, here.
  out = out.replace(/&lt;@[A-Z0-9]+&gt;/g, '<span class="mention">@owner</span>');
  out = out.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, (_, label, url) =>
    `<a href="${url}" target="_blank" rel="noopener noreferrer">${label}</a>`);
  out = out.replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>');
  out = out.replace(/(^|[\s(])\*([^*\n]+)\*(?=[\s).,:;!?]|$)/g, '$1<strong>$2</strong>');
  out = out.replace(/(^|[\s(])_([^_\n]+)_(?=[\s).,:;!?]|$)/g, '$1<em>$2</em>');
  out = out.replace(/(^|[\s(])~([^~\n]+)~(?=[\s).,:;!?]|$)/g, '$1<s>$2</s>');
  out = out.replace(/:([a-z_]+):/g, (all, name) => emoji[name] ?? all);
  return out.replace(/\u0000(\d+)\u0000/g, (_, at) => `<code>${escape(codes[Number(at)])}</code>`);
}

export function render(text: string | null | undefined): string {
  if (!text) return '';
  const lines = text.replace(/\r\n/g, '\n').split('\n');
  const out: string[] = [];
  let list: 'ul' | 'ol' | null = null;
  let quote = false;
  let para: string[] = [];
  const flushPara = () => {
    if (para.length) out.push(`<p>${para.map(inline).join('<br>')}</p>`);
    para = [];
  };
  const closeList = () => {
    if (list) out.push(`</${list}>`);
    list = null;
  };
  const closeQuote = () => {
    if (quote) out.push('</blockquote>');
    quote = false;
  };
  for (let i = 0; i < lines.length; i++) {
    let line = lines[i];
    if (line.trimStart().startsWith('```')) {
      flushPara();
      closeList();
      const body: string[] = [];
      for (i++; i < lines.length && !lines[i].trimStart().startsWith('```'); i++) body.push(lines[i]);
      out.push(`<pre><code>${escape(body.join('\n'))}</code></pre>`);
      continue;
    }
    const quoted = /^(&gt;|>)\s?/.test(line);
    if (quoted !== quote) {
      flushPara();
      closeList();
      if (quoted) out.push('<blockquote>');
      else closeQuote();
      quote = quoted;
    }
    if (quoted) line = line.replace(/^(&gt;|>)\s?/, '');
    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    const bullet = /^\s*[-*•]\s+(.*)$/.exec(line);
    const numbered = /^\s*\d+[.)]\s+(.*)$/.exec(line);
    if (heading) {
      flushPara();
      closeList();
      out.push(`<h4>${inline(heading[2])}</h4>`);
    } else if (bullet || numbered) {
      flushPara();
      const want = bullet ? 'ul' : 'ol';
      if (list !== want) {
        closeList();
        out.push(`<${want}>`);
        list = want;
      }
      out.push(`<li>${inline((bullet || numbered)![1])}</li>`);
    } else if (!line.trim()) {
      flushPara();
      closeList();
    } else {
      if (list) closeList();
      para.push(line);
    }
  }
  flushPara();
  closeList();
  closeQuote();
  return out.join('');
}
