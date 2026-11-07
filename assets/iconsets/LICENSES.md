# Icon sets of the label icon picker

Both sets are packaged from their [Iconify](https://iconify.design) collections
(`@iconify-json/mdi`, `@iconify-json/simple-icons`) as `{set, version, icons: [[name, svg path, aliases?], ...]}`,
gzipped, and served by the daemon at `/assets/iconsets/<set>.json`.

- **mdi**: [Material Design Icons](https://pictogrammers.com/library/mdi/), Apache-2.0
  (https://github.com/Templarian/MaterialDesign/blob/master/LICENSE). Iconify's copy keeps the brand icons
  (`home-assistant`, `github`, ...) that the Pictogrammers package dropped after 5.x.
- **simple-icons**: [Simple Icons](https://simpleicons.org), brand logos, CC0-1.0
  (https://github.com/simple-icons/simple-icons/blob/develop/LICENSE.md). The logos are trademarks of their owners.

To rebuild: `npm pack @iconify-json/mdi @iconify-json/simple-icons`, unpack, and convert each `icons.json`
(keep the single `<path d>` of each icon's `body`, aliases as search words).
