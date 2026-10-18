---
name: Precision Engineering Workbench
colors:
  surface: '#0f131a'
  surface-dim: '#0f131a'
  surface-bright: '#353941'
  surface-container-lowest: '#0a0e15'
  surface-container-low: '#181c23'
  surface-container: '#1c2027'
  surface-container-high: '#262a32'
  surface-container-highest: '#31353d'
  on-surface: '#dfe2ec'
  on-surface-variant: '#bcc9c6'
  inverse-surface: '#dfe2ec'
  inverse-on-surface: '#2d3038'
  outline: '#879390'
  outline-variant: '#3d4947'
  surface-tint: '#6fd8c8'
  primary: '#80e9d9'
  on-primary: '#003731'
  primary-container: '#63cdbd'
  on-primary-container: '#00554c'
  inverse-primary: '#006b5f'
  secondary: '#a6c8ff'
  on-secondary: '#003060'
  secondary-container: '#154780'
  on-secondary-container: '#8eb7f7'
  tertiary: '#cfd8ec'
  on-tertiary: '#283140'
  tertiary-container: '#b3bcd0'
  on-tertiary-container: '#434c5c'
  error: '#ffb4ab'
  on-error: '#690005'
  error-container: '#93000a'
  on-error-container: '#ffdad6'
  primary-fixed: '#8cf5e4'
  primary-fixed-dim: '#6fd8c8'
  on-primary-fixed: '#00201c'
  on-primary-fixed-variant: '#005048'
  secondary-fixed: '#d5e3ff'
  secondary-fixed-dim: '#a6c8ff'
  on-secondary-fixed: '#001c3b'
  on-secondary-fixed-variant: '#154780'
  tertiary-fixed: '#dae3f7'
  tertiary-fixed-dim: '#bec7da'
  on-tertiary-fixed: '#131c2a'
  on-tertiary-fixed-variant: '#3e4757'
  background: '#0f131a'
  on-background: '#dfe2ec'
  surface-variant: '#31353d'
  bg-canvas: '#10141B'
  bg-surface: '#171D27'
  bg-raised: '#202938'
  border-subtle: '#303B4C'
  text-primary: '#E7EDF6'
  text-secondary: '#A8B5C8'
  accent-primary: '#63CDBD'
  status-success: '#7CCF9A'
  status-warning: '#F2C46D'
  status-error: '#F18F93'
  status-info: '#8FB8F8'
  lang-python: '#3572A5'
  lang-cpp: '#F34B7D'
  lang-rust: '#DEA584'
  lang-java: '#B07219'
typography:
  headline-lg:
    fontFamily: Inter
    fontSize: 1.25rem
    fontWeight: '600'
    lineHeight: 1.75rem
    letterSpacing: -0.015em
  headline-md:
    fontFamily: Inter
    fontSize: 1rem
    fontWeight: '600'
    lineHeight: 1.375rem
    letterSpacing: -0.01em
  headline-sm:
    fontFamily: Inter
    fontSize: 0.875rem
    fontWeight: '600'
    lineHeight: 1.25rem
    letterSpacing: -0.005em
  body-lg:
    fontFamily: Inter
    fontSize: 0.875rem
    fontWeight: '400'
    lineHeight: 1.25rem
  body-md:
    fontFamily: Inter
    fontSize: 0.8125rem
    fontWeight: '400'
    lineHeight: 1.125rem
  body-sm:
    fontFamily: Inter
    fontSize: 0.75rem
    fontWeight: '400'
    lineHeight: 1rem
  code-lg:
    fontFamily: JetBrains Mono
    fontSize: 0.875rem
    fontWeight: '400'
    lineHeight: 1.3125rem
  code-md:
    fontFamily: JetBrains Mono
    fontSize: 0.8125rem
    fontWeight: '400'
    lineHeight: 1.1875rem
  code-sm:
    fontFamily: JetBrains Mono
    fontSize: 0.6875rem
    fontWeight: '400'
    lineHeight: 0.9375rem
  label-md:
    fontFamily: Inter
    fontSize: 0.75rem
    fontWeight: '500'
    lineHeight: 1rem
    letterSpacing: 0.02em
  label-sm:
    fontFamily: JetBrains Mono
    fontSize: 0.6875rem
    fontWeight: '500'
    lineHeight: 0.875rem
    letterSpacing: 0.04em
rounded:
  sm: 0.125rem
  DEFAULT: 0.25rem
  md: 0.375rem
  lg: 0.5rem
  xl: 0.75rem
  full: 9999px
spacing:
  gutter: 0.5rem
  gutter-panel: 0.0625rem
  margin: 0.75rem
  margin-screen: 0rem
  space-xs: 0.25rem
  space-sm: 0.5rem
  space-md: 0.75rem
  space-lg: 1rem
  space-xl: 1.5rem
---

## Brand & Style

This design system targets systems architects, security auditors, and core infrastructure engineers analyzing large-scale, multi-language monorepos and distributed systems. The brand character is utilitarian, calm, hyper-precise, and restrained. It prioritizes prolonged ergonomic focus over decorative trends, rejecting superficial chrome, translucent glassmorphism, glowing connectors, and physics-heavy animation loops.

Drawing inspiration from GNOME's `libadwaita` desktop ergonomics and classical industrial reverse-engineering tools (such as SciTools Understand and Source Insight), the interface treats code facts, AST bounds, and relational dependencies with utmost visual honesty. Visual weight is determined by epistemic certainty: deterministic bindings are crisp and grounded; heuristic candidates, unindexed boundaries, and runtime traces are visually distinct. The overarching visual tone remains cool, slate-tinted, and structured around high-density data grids, compact controls, and explicit spatial orientation.

## Colors

The color palette operates in dark mode as its default operational surface to minimize eye fatigue during high-density multi-hour indexing and exploration runs.

- **Canvas & Structure**: `bg-canvas` (`#10141B`) forms the workbench boundary; `bg-surface` (`#171D27`) establishes sidebars, code editor panes, and structural container tiles; `bg-raised` (`#202938`) powers popovers, floating palettes, and focused row selections. Sub-pixel separation relies on `border-subtle` (`#303B4C`).
- **Data & Text**: `text-primary` (`#E7EDF6`) guarantees a minimum contrast ratio of `11:1` on base canvas for dense syntax rendering; `text-secondary` (`#A8B5C8`) serves auxiliary metadata, line numbers, and file hierarchies.
- **Accents & Epistemic Status**: The primary brand accent `accent-primary` (`#63CDBD`, teal) is reserved exclusively for navigation targets, active focus outlines, and confirmed semantic facts.
- **Diagnostics**:
  - `status-success` (`#7CCF9A`): Verified dependencies, clean builds, validated symbols.
  - `status-warning` (`#F2C46D`): Dynamic dispatches, heuristic imports, architectural rule candidates.
  - `status-error` (`#F18F93`): Missing symbols, unlinked ABI boundaries, circular reference violations.
  - `status-info` (`#8FB8F8`): Navigable references, jump anchors, manual annotation markers.
- **Language Badges**: Used strictly on compact pill indicators (`lang-python`, `lang-cpp`, `lang-rust`, `lang-java`) with desaturated, controlled footprints to preserve diagnostic readability.

## Typography

Typography prioritizes information density, tabular precision, and character distinction:
- **UI Shell & Navigation (`Inter`)**: Used across GNOME HeaderBars, navigation trees, inspector properties, and metric tables. The default UI body font size is anchored at `13px` (`0.8125rem`) with an exact `18px` (`1.125rem`) line-height to maintain tight 28px list-item bounding boxes.
- **Code, AST & Graph Syntax (`JetBrains Mono`)**: Applied to editor source buffers, symbol signatures, call graphs, register addresses, and breadcrumbs. Code line-height follows an ergonomic 1.5 ratio (`14px` font on `21px` line height).
- **Tabular & Dense Numbers**: Tabular numbers (`tnum`) and slashed zeros are strictly forced on all monospace and label scales to eliminate horizontal jitter during metric recalculations, code complexity scores, and cyclic dependency indexing.

## Layout & Spacing

The layout is structured around an industrial desktop workbench paradigm (1600×1000 standard resolution), built with a fixed 4px computational grid and zero outer window margins to respect native desktop edges.

- **Primary Workbench Structural Grid**:
  - **HeaderBar**: 46px fixed height, containing window control chips, workspace picker, centered Command Palette / Global Symbol Search (max-width 560px), and analysis mode toggles.
  - **Activity Rail**: 48px fixed width (far left) holding symbol navigation, dependency explorer, call tree, and architecture matrix icons.
  - **Primary Navigator (Left Sidebar)**: 240px default (resizable between 200px and 360px).
  - **Center Split Canvas**: 55:45 split between code editor pane (min-width 440px) and topological dependency graph pane (min-width 360px).
  - **Context Inspector (Right Sidebar)**: 300px default (resizable between 280px and 420px; collapsible).
  - **Bottom Diagnostics Utility Deck**: 240px default height (collapsible, tabbed: Problems, References, Terminal, Indexing Budget).
  - **Status Bar**: 24px fixed height at the base for system performance stats (memory footprint, parsed AST symbols, active threads).
- **Internal Density**: Components utilize `space-xs` (4px) and `space-sm` (8px) for internal padding. List items enforce a rigid 28px height footprint to permit 30+ visible structural symbols per viewport screen.

## Elevation & Depth

Visual hierarchy uses tonal surface layering and low-contrast hairline borders, strictly rejecting multi-colored drop shadows, ambient glow hulls, and glassmorphic blurs.

- **Level 0 (Base Canvas - `#10141B`)**: Window background and primary graph viewport surface.
- **Level 1 (Docked Containers - `#171D27`)**: Editor panes, navigation sidebars, and tab sheets. Separated by a single 1px hairline border of `border-subtle` (`#303B4C`).
- **Level 2 (Raised & Highlighted - `#202938`)**: Active list item selections, hovered graph nodes, active tabs, and toolbar buttons.
- **Level 3 (Overlay & Modal Shelves)**: Autocomplete menus, hover documentation cards, quick-open dialogs, and context menus. These surfaces sit on `#202938`, bounded by a crisp 1px stroke of `#47566C` and backed by a flat, dark ambient drop shadow: `0 4px 16px rgba(0, 0, 0, 0.45)`.
- **Focus Indicators**: Focused inputs, tree nodes, or active code graph nodes discard diffuse glows, using instead a crisp 1px solid stroke in `accent-primary` (`#63CDBD`).

## Shapes

The design system employs a soft, highly compact shape language (`roundedness: 1`) to preserve desktop density and conform with GNOME / libadwaita conventions without sacrificing usable screen real estate:

- **Standard Controls & Badges**: 4px border radius applied to text inputs, buttons, tree item hover states, and language chips.
- **Panels, Flyouts & Floating Toolbars**: 6px border radius (`rounded-lg` equivalent) on popover surfaces, split-view dialogs, and context menus.
- **Graph Nodes**:
  - *Function/Method*: 4px rounded rectangles containing the function identifier and language badge.
  - *Type/Class Containers*: 6px rounded header cards with square internal member lists.
  - *Distributed Contracts*: Hexagonal perimeter or rectangular badge with clipped corners.
  - *SCC Recursion Clusters*: Dashed 6px boundary containment boxes wrapping grouped cycle members.

## Components

### Buttons & Interactive Controls
- **HeaderBar Action Buttons**: Height 28px, 4px corner radius. Transparent background in resting state; `#202938` on hover; `#303B4C` on active press.
- **Primary Action (Run / Apply Filter)**: Solid background `#63CDBD`, text `#102322` (bold 600 weight), 0 1px 2px rgba(0, 0, 0, 0.2).
- **Segmented Mode Switchers**: 28px height, container background `#10141B`, active tab surface `#202938` with 1px border `border-subtle`, text `text-primary`.

### Tree Views & Project Lists
- **Row Anatomy**: Height 28px, single-line clamp, leading 16px symbolic icon, 4px gap, text `text-primary` (13px Inter), trailing line count/symbol count in `text-secondary` (11px JetBrains Mono).
- **Selected State**: Background `#202938`, left accent indicator (2px width, color `#63CDBD`), text `#E7EDF6`.

### Inputs & Global Search Bar
- **Header Command Palette**: Height 32px, max-width 560px, background `#171D27`, border 1px `border-subtle`, leading 16px search glyph. Placeholder text in `#A8B5C8`. On focus: border color switches to `#63CDBD`, with zero box-shadow.
- **Inline Filter Fields**: Height 24px, 4px radius, compact 6px horizontal padding.

### Checkboxes, Radios & Switches
- **Checkboxes**: 14×14px box, 3px corner radius, border 1px `border-subtle`. Checked: `#63CDBD` background with `#102322` checkmark glyph.
- **Switches**: 28×16px pill enclosure, background `#303B4C`. Active thumb (12×12px) moves 12px right, switching track color to `#63CDBD`.

### Language & Provenance Chips
- **Language Chips**: Height 18px, 3px border radius, font `JetBrains Mono` 10px, uppercase, 4px horizontal padding. Background `#171D27`, border 1px matching language token (e.g., `#3572A5` for Python, `#DEA584` for Rust).
- **Epistemic Certainty Chips**:
  - `STATIC`: `#63CDBD` outline, solid dot.
  - `CANDIDATE`: `#F2C46D` outline, dashed border.
  - `UNRESOLVED`: `#F18F93` background tint, warning glyph.

### Cards & Panels
- **Docked Panels**: Background `#171D27`, no outer shadows, border-right or border-bottom 1px solid `border-subtle`. Header strip is 28px high with uppercase 11px label font (`label-sm`).
- **Graph Node Cards**: Background `#171D27`, border 1px solid `border-subtle`. Focus state triggers border `#63CDBD`. Internal fields use 20px rows with syntax-highlighted monospace labels.

### Breadcrumbs & Status Bar
- **Symbol Breadcrumb**: Inline bar directly above the editor buffer; height 24px, background `#171D27`, font 12px `JetBrains Mono`. Chevron separators styled in `text-secondary`.
- **Status Bar**: Height 24px, background `#10141B`, border-top 1px `border-subtle`. Segmented items divided by 12px vertical rules; displays git branch, active parser worker count, indexing memory limit, and current cursor byte offset.

## 2026-10-18 — Implementation override: Symweft / macOS / System appearance

This section supersedes the dark-default palette and GNOME-first implementation assumptions above. HTML/PNG exports remain historical design references.

- Product: **Symweft (织源)**. Start with the native macOS frontend on the current machine. Keep the Rust core portable for later GNOME and Windows frontends.
- Default appearance is **Follow System**, with immediate live updates. Light and dark are equal deliverables; optional explicit overrides are allowed.
- The original near-black/navy surfaces are too dark. Do not hard-code `#0f131a`, `#10141B`, or `#0a0e15` throughout the application.
- Use native semantic shell colors. Custom surfaces may start near `#FAFAFB` in light mode and a lighter neutral `#25272B` in dark mode, with dynamic text and selection colors.
- Validate window, sidebar, source text, inspector, controls and popovers in both appearances, including a live system toggle without restarting.
- Preserve structural ideas from mockups, using native macOS menus, toolbar, typography, focus and accessibility.
