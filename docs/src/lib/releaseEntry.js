const dateFormatter = new Intl.DateTimeFormat('en-US', {
  year: 'numeric',
  month: 'long',
  day: 'numeric',
  timeZone: 'UTC',
});

function escapeXmlText(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;');
}

function escapeXmlAttribute(value) {
  return escapeXmlText(value).replaceAll('"', '&quot;').replaceAll("'", '&apos;');
}

export function formatAtomTimestamp(date) {
  return date.toISOString().replace(/\.\d{3}Z$/, 'Z');
}

export function formatReleaseDate(date) {
  return dateFormatter.format(date);
}

export function renderReleaseArticleHtml({ version, description, date, docsUrl, bodyHtml }) {
  const safeVersion = escapeXmlText(version);
  const safeDescription = escapeXmlText(description);
  const safeDate = escapeXmlText(date);
  const safeDocsUrl = escapeXmlAttribute(docsUrl);

  return [
    '<article>',
    '  <header>',
    `    <h1>v${safeVersion}</h1>`,
    `    <p><em>${safeDescription}</em></p>`,
    '    <p><small>',
    `      Released on ${safeDate} · <a href="${safeDocsUrl}">View in docs</a>`,
    '    </small></p>',
    '  </header>',
    bodyHtml || '',
    '</article>',
  ]
    .filter(Boolean)
    .join('\n');
}

export function renderReleaseAtomEntry({ version, description, date, updated, site, bodyHtml }) {
  const docsUrl = site ? new URL(`/releases/v${version}/`, site).toString() : `/releases/v${version}/`;
  const entryId = site ? new URL(`/releases/v${version}`, site).toString() : `/releases/v${version}`;

  const articleHtml = renderReleaseArticleHtml({
    version,
    description,
    date: formatReleaseDate(date),
    docsUrl,
    bodyHtml,
  });

  return [
    '<entry>',
    `  <title>${escapeXmlText(`v${version}`)}</title>`,
    `  <id>${escapeXmlText(entryId)}</id>`,
    `  <updated>${escapeXmlText(formatAtomTimestamp(updated))}</updated>`,
    '',
    `  <content type="html">${escapeXmlText(articleHtml)}</content>`,
    '</entry>',
  ].join('\n');
}
