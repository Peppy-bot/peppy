import { getCollection } from 'astro:content';
import { renderReleaseAtomEntry } from '../../../lib/releaseEntry.js';

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
  const normalizedSlug =
    typeof slug === 'string' ? `v${slug.replace(/^v/i, '').replaceAll('-', '.')}` : slug;
  const release = releases.find((item) => item.id === normalizedSlug);

  if (!release) {
    return new Response('Not found', { status: 404 });
  }

  const entry = renderReleaseAtomEntry({
    version: release.data.version,
    description: release.data.description,
    date: release.data.date,
    updated: release.data.updated ?? release.data.date,
    site,
    bodyHtml: release.body || '',
  });

  return new Response(entry, {
    headers: {
      'Content-Type': 'application/atom+xml; charset=utf-8',
    },
  });
}
