import { defineCollection } from 'astro:content';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';
import { z } from 'astro/zod';
import { releaseHtmlLoader } from './lib/releaseHtmlLoader.ts';

const docs = defineCollection({ loader: docsLoader(), schema: docsSchema() });

const releases = defineCollection({
  loader: releaseHtmlLoader(),
  schema: z.object({
    version: z.string(),
    date: z.date(),
    updated: z.date().optional(),
    description: z.string(),
  }),
});

export const collections = { docs, releases };
