// Expressive Code syntax themes in the Hora palette (brand/brand.md, section 4).
// One hue for structure (petrol), one warm hue for values (the degraded amber),
// muted ink for comments. Frames and backgrounds come from Starlight's own
// variables (`useStarlightUiThemeColors`), so code blocks sit on the brand
// surfaces in both themes. Expressive Code raises any token below its minimum
// contrast on its own, so these stay readable on every background.

/** @param {string} name @param {'dark' | 'light'} type @param {Record<string, string>} c */
function theme(name, type, c) {
	return {
		name,
		type,
		colors: {
			'editor.background': c.bg,
			'editor.foreground': c.fg,
		},
		tokenColors: [
			{ settings: { foreground: c.fg } },
			{ scope: ['comment', 'punctuation.definition.comment'], settings: { foreground: c.muted, fontStyle: 'italic' } },
			{ scope: ['string', 'string.quoted', 'markup.inline.raw'], settings: { foreground: c.value } },
			{ scope: ['constant.numeric', 'constant.language', 'constant.other', 'support.constant'], settings: { foreground: c.number } },
			{
				scope: [
					'keyword',
					'storage',
					'entity.name.tag',
					'support.type.property-name',
					'meta.object-literal.key',
					'variable.other.property',
					'entity.other.attribute-name',
				],
				settings: { foreground: c.key },
			},
			{ scope: ['entity.name.function', 'support.function', 'entity.name.command', 'support.class'], settings: { foreground: c.call } },
			{ scope: ['entity.name.section', 'entity.name.type', 'markup.heading'], settings: { foreground: c.key, fontStyle: 'bold' } },
			{ scope: ['punctuation', 'meta.brace', 'keyword.operator'], settings: { foreground: c.punct } },
			{ scope: ['variable', 'variable.other'], settings: { foreground: c.fg } },
			{ scope: ['markup.inserted'], settings: { foreground: c.key } },
			{ scope: ['markup.deleted', 'invalid'], settings: { foreground: c.down } },
		],
	};
}

// tokens:begin - generated from brand/tokens.json by brand/src/tokens.py, do not edit
export const horaDark = theme('hora-dark', 'dark', {
	bg: '#1a2029',
	fg: '#d9d6cf',
	muted: '#a8aeb8',
	key: '#63c0cb',
	call: '#a9dde3',
	value: '#d89372',
	number: '#ebc2ae',
	punct: '#a8aeb8',
	down: '#e07a63',
});

export const horaLight = theme('hora-light', 'light', {
	bg: '#faf9f5',
	fg: '#2e3540',
	muted: '#525a66',
	key: '#0e5e68',
	call: '#0a4a52',
	value: '#8f4b22',
	number: '#8f4b22',
	punct: '#525a66',
	down: '#a5372a',
});
// tokens:end
