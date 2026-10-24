# Embedded third-party assets

These files are served from the binary under `/assets/vendor/`. No package manager,
network request, or CDN is needed at runtime. Original distribution files are
unmodified. Source maps (optional developer tools) are not included.

| Component | Version | Distribution | License |
| --- | --- | --- | --- |
| Tabler core CSS + JS | 1.7.1 | https://registry.npmjs.org/@tabler/core/-/core-1.7.1.tgz | MIT, `tabler/LICENSE` |
| Tabler icons webfont CSS + fonts | 3.49.0 | https://registry.npmjs.org/@tabler/icons-webfont/-/icons-webfont-3.49.0.tgz | MIT, `tabler-icons/LICENSE` |
| htmx | 2.0.11 | https://registry.npmjs.org/htmx.org/-/htmx.org-2.0.11.tgz | Zero-Clause BSD, `htmx/LICENSE` |

Tabler's bundle incorporates Bootstrap and Popper; their MIT notices are in
`tabler/LICENSE.bootstrap` and `tabler/LICENSE.popper`. Tabler's npm archive does
not ship its root license: `tabler/LICENSE` was obtained from
https://raw.githubusercontent.com/tabler/tabler/master/LICENSE (2026-10-24).
Bootstrap notice: https://github.com/twbs/bootstrap/blob/v5.3.8/LICENSE.
Popper notice: https://github.com/floating-ui/floating-ui/blob/v2.11.8/LICENSE.md.

To update, fetch pinned npm archives, copy only the distribution files above and
all corresponding license notices, inspect CSS URLs and JS dependencies for
runtime network dependencies, then rerun the asset and handler tests. Update this
inventory and `SHA256SUMS` along with the files.
