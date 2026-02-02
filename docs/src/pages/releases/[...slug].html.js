import { getCollection } from 'astro:content';
import { encodeHTML } from 'entities';

const dateFormatter = new Intl.DateTimeFormat('en-US', {
  year: 'numeric',
  month: 'long',
  day: 'numeric',
  timeZone: 'UTC',
});

export async function getStaticPaths() {
  const releases = await getCollection('releases');
  return releases.map((release) => ({
    params: { slug: `v${release.data.version}` },
  }));
}

export async function GET(context) {
  const { site, params } = context;
  const slug = params.slug;
  const releases = await getCollection('releases');
  const release = releases.find((item) => item.id === slug);

  if (!release) {
    return new Response('Not found', { status: 404 });
  }

  const version = release.data.version;
  const description = release.data.description;
  const publishedTime = release.data.date.toISOString();
  const modifiedTime = (release.data.updated ?? release.data.date).toISOString();
  const dateText = dateFormatter.format(release.data.date);

  const canonical = site ? new URL(`/releases/v${version}.html`, site).toString() : undefined;
  const docsUrl = site ? new URL(`/releases/v${version}/`, site).toString() : `/releases/v${version}/`;
  const title = `v${version} | PeppyOS`;

  const html = [
    '<!doctype html>',
    '<html lang="en">',
    '<head>',
    '<meta charset="utf-8" />',
    '<meta name="viewport" content="width=device-width, initial-scale=1" />',
    `<title>${encodeHTML(title)}</title>`,
    `<meta name="description" content="${encodeHTML(description)}" />`,
    canonical ? `<link rel="canonical" href="${encodeHTML(canonical)}" />` : '',
    `<meta property="og:title" content="${encodeHTML(`v${version}`)}" />`,
    '<meta property="og:type" content="article" />',
    canonical ? `<meta property="og:url" content="${encodeHTML(canonical)}" />` : '',
    `<meta property="og:description" content="${encodeHTML(description)}" />`,
    '<meta property="og:site_name" content="PeppyOS" />',
    `<meta property="article:published_time" content="${encodeHTML(publishedTime)}" />`,
    `<meta property="article:modified_time" content="${encodeHTML(modifiedTime)}" />`,
    '<style>body{font-family:system-ui,-apple-system,Segoe UI,Roboto,Ubuntu,Cantarell,Noto Sans,sans-serif;line-height:1.5;margin:0;padding:2rem;max-width:48rem}a{color:inherit}header{margin-bottom:2rem}h1{margin:0 0 .25rem}small{color:#666}</style>',
    '</head>',
    '<body>',
    '<article>',
    '<header>',
    `<h1>v${encodeHTML(version)}</h1>`,
    `<p><em>${encodeHTML(description)}</em></p>`,
    `<p><small>Released on ${encodeHTML(dateText)} · <a href="${encodeHTML(docsUrl)}">View in docs</a></small></p>`,
    '</header>',
    release.body || '',
    '</article>',
    '</body>',
    '</html>',
  ]
    .filter(Boolean)
    .join('\n');

  return new Response(html, {
    headers: {
      'Content-Type': 'text/html; charset=utf-8',
    },
  });
}

