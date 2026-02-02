const dateFormatter = new Intl.DateTimeFormat('en-US', {
  year: 'numeric',
  month: 'long',
  day: 'numeric',
  timeZone: 'UTC',
});

export function releaseSlugFromVersion(version) {
  return `v${String(version).replaceAll('.', '-')}`;
}

function escapeXmlText(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;');
}

export function formatAtomTimestamp(date) {
  return date.toISOString().replace(/\.\d{3}Z$/, 'Z');
}

export function formatReleaseDate(date) {
  return dateFormatter.format(date);
}

export function renderReleaseArticleHtml({ version, description, date, bodyHtml }) {
  const safeVersion = escapeXmlText(version);
  const safeDescription = escapeXmlText(description);
  const safeDate = escapeXmlText(date);

  return [
    '<article>',
    '  <header>',
    `    <h1>v${safeVersion}</h1>`,
    `    <p><em>${safeDescription}</em></p>`,
    '    <p><small>',
    `      Released on ${safeDate}`,
    '    </small></p>',
    '  </header>',
    bodyHtml || '',
    '</article>',
  ]
    .filter(Boolean)
    .join('\n');
}

export function renderReleaseAtomEntry({ version, description, date, updated, site, bodyHtml }) {
  const slug = releaseSlugFromVersion(version);
  const docsUrl = site ? new URL(`/releases/${slug}/`, site).toString() : `/releases/${slug}/`;
  const entryId = docsUrl;

  const articleHtml = renderReleaseArticleHtml({
    version,
    description,
    date: formatReleaseDate(date),
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
