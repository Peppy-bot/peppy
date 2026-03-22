// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLlmsTxt from 'starlight-llms-txt';
import { decodeHTML } from 'entities';
import { fileURLToPath } from 'node:url';
import { basename } from 'node:path';
import { readFileSync } from 'node:fs';

const apptainerGrammar = JSON.parse(
	readFileSync(new URL('./src/grammars/apptainer.tmLanguage.json', import.meta.url), 'utf-8')
);

function releaseHtmlEntryType() {
	function parseAtomEntry(source, fileUrl) {
		const meta = {};
		const filePath = fileURLToPath(fileUrl);
		const fileBase = basename(filePath);
		const idFromFile = fileBase.replace(/\.html$/i, '');
		meta.version = idFromFile.replace(/^v/i, '');

		const getTagText = (tag) => {
			const match = source.match(new RegExp(`<${tag}\\b[^>]*>([\\s\\S]*?)<\\/${tag}>`, 'i'));
			return match ? decodeHTML((match[1] ?? '').trim()) : '';
		};

		const publishedText = getTagText('published');
		const updatedText = getTagText('updated');
		const summaryText = getTagText('summary');
		const contentMatch = source.match(/<content\b[^>]*>([\s\S]*?)<\/content>/i);
		const contentText = contentMatch ? decodeHTML((contentMatch[1] ?? '').trim()) : '';

		const dateText = publishedText || updatedText;
		const parsedDate = new Date(dateText);
		if (!dateText || Number.isNaN(parsedDate.valueOf())) {
			throw new Error(`Invalid Atom date "${dateText}" in ${filePath}`);
		}
		meta.date = parsedDate;

		if (updatedText) {
			const parsedUpdated = new Date(updatedText);
			if (Number.isNaN(parsedUpdated.valueOf())) {
				throw new Error(`Invalid Atom updated date "${updatedText}" in ${filePath}`);
			}
			meta.updated = parsedUpdated;
		}

		meta.description = summaryText;
		if (!meta.description && contentText) {
			const emMatch = contentText.match(/<em>([\s\S]*?)<\/em>/i);
			if (emMatch) {
				meta.description = decodeHTML((emMatch[1] ?? '').trim());
			}
		}

		let body = contentText;
		const articleMatch = body.match(/<article\b[^>]*>([\s\S]*?)<\/article>/i);
		if (articleMatch) {
			const articleInner = articleMatch[1] ?? '';
			const headerEndIndex = articleInner.toLowerCase().indexOf('</header>');
			body =
				headerEndIndex === -1
					? articleInner
					: articleInner.slice(headerEndIndex + '</header>'.length);
		}
		body = body.trimStart();

		return { data: meta, body };
	}

	return {
		name: 'peppy-html-meta',
		hooks: {
			'astro:config:setup': ({ addDataEntryType }) => {
				addDataEntryType({
					extensions: ['.html'],
					getEntryInfo({ contents, fileUrl }) {
						const source = contents ?? '';
						const trimmedSource = source.trimStart();
						if (trimmedSource.startsWith('<entry')) {
							return parseAtomEntry(trimmedSource, fileUrl);
						}

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
			plugins: [starlightLlmsTxt()],
			expressiveCode: {
				shiki: {
					langs: [{ ...apptainerGrammar, name: 'apptainer' }],
				},
			},
			title: 'PeppyOS',
			customCss: ['./src/styles/custom.css'],
			components: {
				ThemeSelect: './src/components/ThemeSelect.astro',
			},
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/orgs/Peppy-bot/repositories' }],
			sidebar: [
				{ slug: 'index' },
				{
					label: 'Guides',
					autogenerate: { directory: 'guides' },
				},
				{
					label: 'Advanced Guides',
					autogenerate: { directory: 'advanced_guides' },
				},
				{
					label: 'Reference',
					autogenerate: { directory: 'reference' },
				},
			],
		}),
	],
});
