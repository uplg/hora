// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// Deployed as a GitHub Pages project site: https://uplg.github.io/hora/
export default defineConfig({
	site: 'https://uplg.github.io',
	base: '/hora',
	integrations: [
		starlight({
			title: 'Hora',
			description:
				'A tiny, self-hosted uptime monitor: one small binary, a status page, alerts that never wake you up for flapping.',
			logo: { src: './src/assets/logo.svg' },
			favicon: '/favicon.svg',
			customCss: ['./src/styles/custom.css'],
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/uplg/hora' },
			],
			editLink: {
				baseUrl: 'https://github.com/uplg/hora/edit/main/docs/',
			},
			sidebar: [
				{
					label: 'Start here',
					items: [
						{ label: 'Getting started', slug: 'getting-started' },
						{ label: 'Configuration', slug: 'configuration' },
						{ label: 'Upgrading', slug: 'upgrading' },
					],
				},
				{
					label: 'Guides',
					items: [
						{ label: 'Monitors', slug: 'guides/monitors' },
						{ label: 'Alerting & notifications', slug: 'guides/alerting' },
						{ label: 'SLOs & error budgets', slug: 'guides/slo' },
						{ label: 'Incidents & history', slug: 'guides/incidents' },
						{ label: 'Per-group pages & SLA reports', slug: 'guides/multi-tenant' },
						{ label: 'Mutual surveillance (peers)', slug: 'guides/peers' },
						{ label: 'Importing from Uptime Kuma', slug: 'guides/import' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'CLI', slug: 'reference/cli' },
						{ label: 'HTTP API', slug: 'reference/api' },
					],
				},
				{
					label: 'Project',
					items: [
						{ label: 'Roadmap', slug: 'roadmap' },
						{
							label: 'Changelog',
							link: 'https://github.com/uplg/hora/blob/main/CHANGELOG.md',
						},
					],
				},
			],
		}),
	],
});
