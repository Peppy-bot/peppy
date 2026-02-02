import { defineCollection, z } from 'astro:content';
import { docsSchema } from '@astrojs/starlight/schema';
import { glob } from 'astro/loaders';

const docs = defineCollection({ schema: docsSchema() });

const releases = defineCollection({
  loader: glob({
    pattern: '**/*.md',
    base: './src/content/releases',
    generateId: ({ entry }) => entry.replace(/\.md$/, ''),
  }),
  schema: z.object({
    version: z.string(),
    date: z.date(),
    updated: z.date().optional(),
    description: z.string(),
  }),
});

export const collections = { docs, releases };
