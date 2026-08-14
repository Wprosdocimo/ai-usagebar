# Omarchy Quattro plugin

This is the native Omarchy 4 frontend for ai-usagebar. It runs inside
Quattro's long-lived Quickshell process and uses the shared Omarchy UI kit for
the bar button, keyboard-aware panel, hero, controls, typography, spacing,
colors, borders, and popup placement.

The plugin is deliberately a display layer. It executes the fixed command
`ai-usagebar usage --json`; the Rust binary remains the only code that reads
credentials, talks to providers, manages refresh locks, and writes caches.

## Install

Install `ai-usagebar` first, then install this repository as the plugin:

```bash
yay -S ai-usagebar-bin
omarchy plugin add https://github.com/akitaonrails/ai-usagebar.git --enable
```

Omarchy clones plugin repositories into `~/.config/omarchy/plugins/`. The root
[manifest](../manifest.json) loads `omarchy/BarWidget.qml`, which owns the bar
button and loads `Panel.qml` inside the same plugin. Update or remove it with
the normal plugin commands:

```bash
omarchy plugin update akitaonrails.ai-usagebar
omarchy plugin remove akitaonrails.ai-usagebar
```

## Controls

- Bar: left-click opens the panel; right-click launches
  `ai-usagebar-tui`; middle-click or the mouse wheel switches provider.
- Panel: `h`/`l` or Left/Right switches provider, `j`/`k` or Up/Down scrolls,
  `r`, Enter, or Space refreshes, Tab moves to the neighboring bar panel, and
  Esc closes.
- Shell: `omarchy-shell shell summon akitaonrails.ai-usagebar '{}'` opens the
  panel and `omarchy-shell shell hide akitaonrails.ai-usagebar` closes it.

The panel keeps the last successful report visible when a refresh fails and
labels it accordingly. Provider-level stale cache responses and hard errors
are shown inline. Absolute reset timestamps are rendered as live countdowns,
so an open panel stays accurate between network refreshes.

## Settings

Settings are stored inline in `~/.config/omarchy/shell.json` and can be changed
through Omarchy's bar UI or CLI:

```bash
# Show only one entry. Use an id printed by `ai-usagebar usage --json`.
omarchy bar set akitaonrails.ai-usagebar provider openai
omarchy bar set akitaonrails.ai-usagebar provider anthropic@work

# Empty means all configured entries, with switching in the panel.
omarchy bar set akitaonrails.ai-usagebar provider ''

# Numeric values need --json so shell.json stores a number.
omarchy bar set akitaonrails.ai-usagebar refreshIntervalSec 300 --json
```

The refresh interval is clamped to 30–3600 seconds. The `provider` setting
prefers an exact entry id; if there is no exact match, a base id such as
`anthropic` selects all accounts for that provider.

## Development checks

On an Omarchy 4 machine:

```bash
omarchy plugin validate .
qmllint -U -I /usr/share/omarchy/shell omarchy/BarWidget.qml omarchy/Panel.qml omarchy/Model.js
node omarchy/model.test.mjs
```

Saving files under an installed user plugin triggers Quattro's plugin hot
reload. In a source checkout, rerun `omarchy plugin validate .` after changing
the manifest or entry points.
