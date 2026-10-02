---
name: subpool dashboard
description: Black, warm-gray operator panel for a local AI API proxy, in the slopalytics.com palette.
colors:
  bg: "#000000"
  raise: "#0b0a09"
  raise-hover: "#151311"
  line: "#1c1a18"
  line-strong: "#2f2b27"
  line-hover: "#4a4540"
  fg: "#f1efed"
  fg-2: "#aaa39d"
  fg-3: "#857e78"
  accent: "#c5ac98"
  accent-hi: "#dcc6b4"
  ok: "#56ca80"
  warn: "#e0b46c"
  err: "#ef6f6c"
  theme-color: "#24020a"
  provider-claude: "#cc785c"
  provider-codex: "#c5cbd3"
  provider-gemini: "#56ca80"
  provider-compat: "#9b94ee"
typography:
  body:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
  title:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "22px"
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: "-0.015em"
  section:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 600
    lineHeight: 1.3
  label:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "12px"
    fontWeight: 500
    lineHeight: 1.4
  figure:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "20px"
    fontWeight: 500
    lineHeight: 1.2
    letterSpacing: "-0.02em"
  figure-compact:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "17px"
    fontWeight: 500
    lineHeight: 1.2
  data:
    fontFamily: "ui-monospace, SF Mono, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "12.5px"
    fontWeight: 400
    lineHeight: 1.5
rounded:
  hair: "1.5px"
  bar: "2px"
  xs: "4px"
  tag: "5px"
  sm: "6px"
  seg-inner: "7px"
  md: "8px"
  seg: "9px"
  lg: "12px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "20px"
  xl: "32px"
  xxl: "48px"
components:
  button:
    backgroundColor: "{colors.bg}"
    textColor: "{colors.fg}"
    rounded: "{rounded.md}"
    height: "32px"
    padding: "0 12px"
  button-hover:
    backgroundColor: "{colors.raise-hover}"
  button-primary:
    backgroundColor: "{colors.fg}"
    textColor: "{colors.bg}"
    rounded: "{rounded.md}"
    height: "32px"
    padding: "0 14px"
  input:
    backgroundColor: "{colors.raise}"
    textColor: "{colors.fg}"
    rounded: "{rounded.md}"
    height: "34px"
    padding: "0 10px"
  code-block:
    backgroundColor: "{colors.raise}"
    textColor: "{colors.fg-2}"
    typography: "{typography.data}"
    rounded: "{rounded.lg}"
    padding: "14px 16px"
---

## Overview

An operator panel in the palette of slopalytics.com: a pure black canvas, warm gray text, and a single tan accent (`#c5ac98`) that marks what is active: the "sub" in the wordmark, the current tab underline, links, the primary button and the traffic line. Mobile browsers tint their chrome deep maroon (`#24020a`) through `theme-color`. Color beyond the accent is reserved for state (ok / warn / err) and provider dots. No cards: sections are separated by space and hairlines.

## Colors

- `bg` #000 everywhere. `raise` (#0b0a09) only for inputs, code and inline panels.
- Text ramp is warm: `fg` for values and titles, `fg-2` for body, `fg-3` for labels and metadata.
- `accent` tan is the one brand color. `accent-hi` is its hover state and the value color in code snippets.
- Provider dots follow the slopalytics chart series: Claude clay, Codex cool gray, Gemini green, OpenAI-compatible violet.

## Chart

Traffic is a line chart, not bars: tan line and dots for successful requests, a faded dashed red line for failures, dotted horizontal gridlines with y labels at 0, half and max, and a white dot for the current minute.

## Typography

One system sans family for all UI; monospace only for real code and data (endpoints, model ids, keys, config, numbers in logs). All numbers use `font-variant-numeric: tabular-nums`. Headings are light (weight 400) and large: 26px page titles, 20px section titles, 22px wordmark.

## Layout

Single column, max width 1180px, 32px side padding (16px on mobile). Sticky 64px top bar with tabs. More space above a section title (32–40px) than below it (12px). Overview splits into a 3:2 two-column grid above 900px.

## Elevation & Depth

Flat. Depth comes only from `raise` surfaces and 1px `line` hairlines. No shadows on the page; the one floating element (copy feedback) uses a soft offset shadow.

## Shapes

8px radius for controls, 12px for code blocks and inline panels, full round for status dots (6px). Small radii exist only where the element is small: 2px/1.5px traffic bars, 4px focus ring and skeleton lines, 5px tags, 9px/7px segmented control and its buttons.

## Components

- Buttons: outline (line-strong border) by default; one white-filled primary per view at most; ghost buttons for row actions.
- Tables: 12px fg-3 headers, 40px rows, 1px line separators, row hover `#070707`.
- Status: dot + word ("Ready", "Cooling 4:12", "Disabled", "Error").
- Live rows: a new request row lights up at full white and settles to its resting luminance over 1.8s (the one authored motion; disabled for reduced motion).

## Do's and Don'ts

- Do keep the background pure #000 and use tan for at most one thing per region.
- Do show time-to-recovery for cooling accounts.
- Don't use glows, gradients, glass, or colored side borders.
- Don't use monospace for labels or headings.
- Don't put more than one primary (white) button in a view.
