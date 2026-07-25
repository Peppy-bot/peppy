// @ts-check
import { defineConfig } from 'astro/config';
import { unified } from '@astrojs/markdown-remark';
import starlight from '@astrojs/starlight';
import tailwindcss from '@tailwindcss/vite';
import starlightLlmsTxt from 'starlight-llms-txt';
import remarkGfm from 'remark-gfm';
import { readFileSync } from 'node:fs';

const apptainerGrammar = JSON.parse(
	readFileSync(new URL('./src/grammars/apptainer.tmLanguage.json', import.meta.url), 'utf-8')
);

// https://astro.build/config
export default defineConfig({
	site: 'https://docs.peppy.bot',
	// GFM (tables, strikethrough, autolinks) is not applied to .mdx files unless
	// remark-gfm is registered explicitly here; the MDX integration inherits it.
	markdown: {
		processor: unified({ remarkPlugins: [remarkGfm] }),
	},
	vite: {
		plugins: [tailwindcss()],
	},
	integrations: [
		starlight({
			plugins: [starlightLlmsTxt()],
			expressiveCode: {
				shiki: {
					langs: [{ ...apptainerGrammar, name: 'apptainer' }],
				},
				// Harmonize code/terminal frames with the brand: one cohesive
				// window (title bar and body share a surface, separated only by a
				// hairline), brand radius, and the JetBrains Mono code font.
				styleOverrides: {
					borderRadius: '0.5rem',
					borderColor: 'var(--sl-color-hairline)',
					codeFontFamily: "'JetBrains Mono', ui-monospace, monospace",
					frames: {
						editorBackground: 'var(--sl-color-gray-6)',
						editorActiveTabBackground: 'var(--sl-color-gray-6)',
						editorTabBarBackground: 'var(--sl-color-gray-5)',
						terminalBackground: 'var(--sl-color-gray-6)',
						terminalTitlebarBackground: 'var(--sl-color-gray-6)',
						terminalTitlebarBorderBottomColor: 'var(--sl-color-hairline)',
						terminalTitlebarDotsForeground: 'var(--sl-color-gray-4)',
						terminalTitlebarForeground: 'var(--sl-color-gray-3)',
					},
				},
			},
			title: 'Peppy',
			favicon: '/favicon.png',
			head: [
				{ tag: 'link', attrs: { rel: 'preconnect', href: 'https://fonts.googleapis.com' } },
				{
					tag: 'link',
					attrs: { rel: 'preconnect', href: 'https://fonts.gstatic.com', crossorigin: true },
				},
				{
					tag: 'link',
					attrs: {
						rel: 'stylesheet',
						href: 'https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;700&display=swap',
					},
				},
			],
			// tailwind.css must load before custom.css so the brand layer wins.
			customCss: ['./src/styles/tailwind.css', './src/styles/custom.css'],
			components: {
				SiteTitle: './src/components/SiteTitle.astro',
				ThemeSelect: './src/components/ThemeSelect.astro',
			},
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/Peppy-bot/peppy' }],
			sidebar: [
				{ slug: 'index' },
				// Standalone entry, deliberately outside the Guides group: it is a
				// zero-to-running demo, not part of the build-your-own-node sequence.
				{ slug: 'quickstart' },
				{
					label: 'Guides',
					items: [{ autogenerate: { directory: 'guides' } }],
				},
				{
					label: 'Advanced Guides',
					items: [{ autogenerate: { directory: 'advanced_guides' } }],
				},
				{
					label: 'Reference',
					items: [{ autogenerate: { directory: 'reference' } }],
				},
			],
		}),
	],
});
