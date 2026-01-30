// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
	site: 'https://docs.peppy.bot',
	integrations: [
		starlight({
			title: 'PeppyOS',
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/Peppy-bot/peppy' }],
			customCss: ['./src/styles/custom-theme.css'],
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
