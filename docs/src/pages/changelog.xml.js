import rss from '@astrojs/rss';
import { getCollection } from 'astro:content';

export async function GET(context) {
  const releases = await getCollection('releases');

  return rss({
    title: 'PeppyOS Changelog',
    description: 'Release notes and version history for PeppyOS',
    site: context.site,
    items: releases
      .sort((a, b) => b.data.date.valueOf() - a.data.date.valueOf())
      .map((release) => ({
        title: `v${release.data.version}`,
        pubDate: release.data.date,
        description: release.data.description,
        link: `/reference/changelog/#v${release.data.version.replace(/\./g, '')}`,
      })),
  });
}
