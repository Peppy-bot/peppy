// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { decodeHTML } from 'entities';
import { fileURLToPath } from 'node:url';

function releaseHtmlEntryType() {
	return {
		name: 'peppy-html-meta',
		hooks: {
			'astro:config:setup': ({ addDataEntryType }) => {
				addDataEntryType({
					extensions: ['.html'],
					getEntryInfo({ contents, fileUrl }) {
						const source = contents ?? '';
						const meta = {};
						let cursor = 0;
						while (true) {
							const match = source.slice(cursor).match(/^[\s\r\n]*<meta\b([^>]*?)\/?>/i);
							if (!match) break;
							const fullTag = match[0];
							const attrs = match[1] ?? '';
							const nameMatch = attrs.match(/\bname\s*=\s*(?:"([^"]*)"|'([^']*)')/i);
							if (!nameMatch) break;
							const name = (nameMatch[1] ?? nameMatch[2] ?? '').trim();
							if (!name.toLowerCase().startsWith('peppy:')) break;

							const contentMatch = attrs.match(/\bcontent\s*=\s*(?:"([^"]*)"|'([^']*)')/i);
							const rawContent = contentMatch ? contentMatch[1] ?? contentMatch[2] ?? '' : '';
							const content = decodeHTML(rawContent);

							const key = name.slice('peppy:'.length).toLowerCase();
							if (key === 'version') {
								meta.version = content;
							} else if (key === 'description') {
								meta.description = content;
							} else if (key === 'date' || key === 'updated') {
								const parsed = new Date(content);
								if (Number.isNaN(parsed.valueOf())) {
									throw new Error(
										`Invalid peppy:${key} date "${content}" in ${fileURLToPath(fileUrl)}`
									);
								}
								meta[key] = parsed;
							}

							cursor += fullTag.length;
						}

						const body = source.slice(cursor).trimStart();
						return { data: meta, body };
					},
				});
			},
		},
	};
}

// https://astro.build/config
export default defineConfig({
	site: 'https://docs.peppy.bot',
	integrations: [
		releaseHtmlEntryType(),
		starlight({
			title: 'PeppyOS',
			customCss: ['./src/styles/custom.css'],
			components: {
				ThemeSelect: './src/components/ThemeSelect.astro',
			},
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/orgs/Peppy-bot/repositories' }],
			sidebar: [
				{
					label: 'Guides',
					autogenerate: { directory: 'guides' },
				},
				{
					label: 'Reference',
					autogenerate: { directory: 'reference' },
				},
			],
		}),
	],
});
