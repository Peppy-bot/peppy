import { getCollection } from 'astro:content';
import { encodeXML } from 'entities';
import { marked } from 'marked';

export async function GET(context) {
  if (!context.site) {
    throw new Error(
      'Missing `site` config. Set `site` in `docs/astro.config.mjs` to generate the changelog Atom feed.'
    );
  }

  const releases = await getCollection('releases');
  const sortedReleases = releases.sort((a, b) => b.data.date.valueOf() - a.data.date.valueOf());
  const authorName = 'PeppyOS';
  const feedUpdated =
    sortedReleases.length === 0
      ? new Date()
      : sortedReleases.reduce((latest, release) => {
          const updated = release.data.updated ?? release.data.date;
          return updated > latest ? updated : latest;
        }, sortedReleases[0].data.updated ?? sortedReleases[0].data.date);

  const feedUrl = new URL('/changelog.xml', context.site);
  const changelogUrl = new URL('/reference/changelog/', context.site);

  const entriesXml = sortedReleases
    .map((release) => {
      const version = release.data.version;
      const entryUrl = new URL(`/releases/v${version}/`, context.site);
      const publishedIso = release.data.date.toISOString();
      const updatedIso = (release.data.updated ?? release.data.date).toISOString();
      const contentHtml = marked.parse(release.body || '');
      return [
        '<entry>',
        `<title>${encodeXML(`v${version}`)}</title>`,
        `<author><name>${encodeXML(authorName)}</name></author>`,
        `<id>${encodeXML(entryUrl.toString())}</id>`,
        `<link rel="alternate" type="text/html" href="${encodeXML(entryUrl.toString())}" />`,
        `<published>${publishedIso}</published>`,
        `<updated>${updatedIso}</updated>`,
        `<summary>${encodeXML(release.data.description)}</summary>`,
        `<content type="html">${encodeXML(contentHtml)}</content>`,
        '</entry>',
      ].join('');
    })
    .join('');

  const atomXml = [
    '<?xml version="1.0" encoding="utf-8"?>',
    '<feed xmlns="http://www.w3.org/2005/Atom">',
    '<title>PeppyOS Changelog</title>',
    '<subtitle>Release notes and version history for PeppyOS</subtitle>',
    `<author><name>${encodeXML(authorName)}</name></author>`,
    `<id>${encodeXML(feedUrl.toString())}</id>`,
    `<link rel="self" type="application/atom+xml" href="${encodeXML(feedUrl.toString())}" />`,
    `<link rel="alternate" type="text/html" href="${encodeXML(changelogUrl.toString())}" />`,
    `<updated>${feedUpdated.toISOString()}</updated>`,
    entriesXml,
    '</feed>',
  ].join('');

  return new Response(atomXml, {
    headers: {
      'Content-Type': 'application/atom+xml; charset=utf-8',
    },
  });
}
