import { defineCollection, z } from 'astro:content';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';
import { glob } from 'astro/loaders';

const docs = defineCollection({ loader: docsLoader(), schema: docsSchema() });

const releases = defineCollection({
  loader: glob({
    pattern: '**/*.html',
    base: './src/content/releases',
    generateId: ({ entry }) => entry.replace(/\.html$/, ''),
  }),
  schema: z.object({
    version: z.string(),
    date: z.date(),
    updated: z.date().optional(),
    description: z.string(),
  }),
});

export const collections = { docs, releases };
