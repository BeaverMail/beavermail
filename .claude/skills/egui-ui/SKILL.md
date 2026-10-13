---
name: egui-ui
description: Design-studio quality UX/UI for the BeaverMail egui desktop client (crates/app). Use for ANY GUI work - panels, dialogs, forms, lists, buttons, theme, fonts, icons, shortcuts. Default egui look is never acceptable; everything must look like a polished modern product.
---

# BeaverMail UI: the bar

The target is a product a design studio would ship. Default egui widgets (grey boxes, cramped rows, tiny labels, borderless inputs, a dialog with empty space) are a failure, not a starting point. Every screen must pass the **checklist at the end** before it is called done.

All colours, radii, spacing and type sizes live in `crates/app/src/theme.rs` (`Tokens`, `apply`). Never write `Color32::from_rgb`, a magic radius or a one-off font size in a view. If a token is missing, add it there.

## Principles (in order)

1. **Clarity over decoration.** One obvious primary action per screen. Everything else is quieter.
2. **Hierarchy through contrast, size and weight, not boxes.** Title > section > label > hint. Use dim text (`text_dim`) for secondary info, semibold for emphasis; avoid extra borders and separators.
3. **Generous, consistent whitespace.** Cramped is the most common way egui apps look amateur.
4. **Everything has a state.** Default, hover, pressed, focus, disabled, loading, empty, error. None may look identical to another.
5. **Quiet motion and feedback.** Immediate response to every input; never a frozen window.

## Geometry contract

One set of named metrics in `theme.rs`; views never invent sizes. Values: control height **40**, touch target 44, corner radius 8, content inset 12, icon 18, sidebar ~230, list pane ~390, toolbar 64, divider 1. A type scale of few named steps (micro / caption / body / subtitle / heading / title), never ad-hoc sizes. Semantic, theme-stable icon colours per mailbox (inbox blue, starred amber, trash red, archive violet...), with a monochrome option.

**egui trap:** `TextEdit` ignores `interact_size` and renders ~20px tall. Always build fields with `sized_field()` (`min_size` = control height, 12px side margin, vertically centred). Buttons and combo boxes follow `interact_size`, which `apply` sets to the control height. After any new widget, check it is as tall as its neighbours.

## Visual system

- **Surfaces:** `bg` (app), `surface` (windows, cards, menus), `input` (fields). A window must be visibly lifted from what is behind it: `surface` fill, 1px `border`, large soft shadow, radius 14.
- **Inputs must look like inputs:** visible 1px `border_strong` at rest (not borderless dark-on-dark), `accent` border on focus, `input` fill, height = control height (40), readable placeholder in `text_dim` (>= 4.5:1 against the field in both themes).
- **Accent:** one accent colour, used only for the primary action, focus, selection and links. Primary button = filled accent + `on_accent` text. Secondary = bordered `surface`. Destructive = `danger`.
- **Neutrals:** slightly tinted greys (not pure black/white). Dark theme is its own design: raised surfaces are lighter than the background, borders are subtle, the accent is lighter than in light mode.
- **Radii:** 8 controls, 10 menus, 14 windows. Borders 1px. No double borders.
- **Spacing:** 4px base unit. `item_spacing` 8, `button_padding` (16, 8), control height 40, window margin 20, 8/12/16/24 between groups. Group related fields tightly, separate groups clearly.
- **Type:** one UI font (Inter, bundled in `crates/app/assets/fonts`). Body 15, small 13, heading 22 semibold. Never below 12. Sentence case everywhere ("Add account", not "Add Account"). Monospace only for raw headers/source.
- **Icons:** one consistent set (Lucide-style, 1.5px stroke), 16px inline / 20px toolbar, tinted from tokens. Icon-only buttons need a tooltip.
- **Contrast:** text >= 4.5:1, UI boundaries >= 3:1, in light and dark. Never convey state by colour alone.

## Layout

- Main window: accounts/mailboxes (left, ~240, `bg`), message list (centre, `surface`), reader (right, wide, `surface`). Resizable panels with sane min widths; the app must not break at 900px.
- List rows: ~56-64px, two or three lines (sender + date, subject, dim snippet), avatar/initial optional, unread = semibold + accent dot, selected = accent-tinted fill with rounded corners and horizontal inset. Hover feedback on every row. Use `ScrollArea::show_rows`.
- Reader: constrained reading width (~720px max, centred), 15-16px body, 1.5 line height, header block with sender, recipients, date, subject as a heading.
- Toolbars: grouped icon buttons, 40px targets, 8px gaps, dividers only between groups.

## Dialogs and forms

- Centered, `title_bar(false)`, fixed width ~460, window margin 20. Inside: **heading (20 semibold) + one dim sentence of context**, then the form, then the action row. A dialog never has dead space; check height.
- Action row is its own fixed-height row (`allocate_ui_with_layout(vec2(width, CONTROL_H), right_to_left)`), never a bare `with_layout`, which swallows the remaining height.
- Primary action on the right, filled accent, verb + object ("Add account"); Cancel to its left. Enter confirms when valid, Esc cancels. Primary is disabled until valid, with a tooltip saying why. First field is focused on open.
- Labels are plain nouns ("Server", "Port"), sections have a semibold title ("Incoming mail (IMAP)"), not label prefixes repeated on every row. Fields in a column share one width. Short fields (port) stay short.
- Validate inline, next to the field, in `danger` with an icon; never only a generic banner at the bottom. Use sensible defaults and autodetect hints so the user types as little as possible.
- Secrets: password fields with a show/hide toggle; never log or echo them.

## Interaction

- Each user action is a named command (id, label, shortcut, enabled) shared by toolbar, menu, shortcut and plugins.
- Keyboard first: arrows move through lists, Enter opens, Delete/Archive/Reply have shortcuts, shown in tooltips (`Ctrl+X` on Linux/Windows, `⌘X` on macOS).
- Disabled controls are dimmed, never hidden, and explain why on hover.
- Mail calls are async: show skeleton or spinner after ~150ms, keep old content visible while refreshing, show errors inline with a Retry.
- Empty, loading and error states are designed (icon or illustration, one-line explanation, one action), not blank panes.

## egui mechanics (egui 0.36)

- **Layout:** `MenuBar` in `Panel::top`, status line in `Panel::bottom`, content in `CentralPanel`. Side panels `resizable(true)` with `min_size`/`max_size`; anything that can grow sits in a `ScrollArea`. Advanced options go in a `CollapsingHeader` or settings window, not the main screen. Below a chosen width, stack instead of side by side. Frames/separators mark real groups only, never every section. Prose lines 45-75 chars (`set_max_width`).
- **Both themes:** `ctx.set_visuals_of(Theme::Dark/Light, ..)` (see `theme::apply`). Semantic colours come from `Visuals` (`error_fg_color`, `warn_fg_color`, `hyperlink_color`, `selection`, `faint_bg_color` for striped rows), mapped from our tokens. Follow the OS (`ThemePreference::System`).
- **Primary action:** the one primary button per group uses the accent fill (`selection`/`Tokens::accent`); dialog buttons keep the same order everywhere (in `right_to_left` the first added is the rightmost).
- **Constrain input** instead of validating after: `DragValue::range`, `Slider`, `ComboBox` for fixed sets.
- **Destructive actions** (delete, empty trash, remove account) ask in an `egui::Modal` naming the action ("Delete 3 messages") or are undoable (`Undoer`). Blocking dialogs should be `egui::Modal`: `should_close()` handles Esc and backdrop click.
- **Shortcuts:** one `KeyboardShortcut` const (`Modifiers::COMMAND`), consumed with `input_mut(|i| i.consume_shortcut(..))`, shown in menus via `Button::shortcut_text(ctx.format_shortcut(..))`.
- **Slow work** runs off the UI thread; show `Spinner`/`ProgressBar` only while it runs (they repaint every frame).
- **Keyboard and focus:** Tab order follows reading order; focus ring stays visible in both themes after restyling; custom widgets from `allocate_response(.., Sense::click())` take focus; hover text is only extra help (touch cannot hover). Layout must survive 150% zoom.
- **Text:** sizes in points via `TextStyle` only, set once in `apply`; 3-4 sizes per screen; never `Small` for primary content; never multiply by DPI. Default fonts lack CJK: add via `ctx.set_fonts`. Strings can grow 30-40% (translations): no fixed-size `add_sized` on text. egui has no bidi: test real Arabic/Hebrew before promising RTL. Keep user-visible strings in one place.
- **Motion:** `animation_time` 0.1-0.2s; animate to show change (`animate_bool_responsive`), not as decoration; honour an app "reduce motion" setting with `animation_time = 0`.

## Wording

- Name things as users think ("Notifications", not "Webhook config"). Buttons say the action ("Save changes", "Add account"), never "OK"/"Submit"/"Yes".
- One action, one name everywhere (menu, button, status: "Export" -> "Exported 120 rows").
- Errors say what happened and what to do, in the danger colour: "Could not connect to imap.example.com:993. Check the server and port." No apologies, no "Something went wrong", never `{err:?}` alone.
- Empty views say what to do next ("No accounts yet" + "Add account" button).
- One capitalisation style (sentence case). A widget's text is its accessible name: when renaming a button, update tests that find it by label.

## Accessibility (WCAG 2.2)

Contrast 4.5:1 text, 3:1 large text and UI parts, re-checked in both themes after any colour change. Colour never alone. Everything works from the keyboard with visible focus. Icon-only buttons carry text/accessible name; custom widgets set `widget_info(|| WidgetInfo::labeled(..))`. No clipping at 150% zoom.

## Mail-specific

- Reader renders untrusted content: remote images/scripts blocked by default with a "Load images" bar.
- Dates are relative in lists ("14:32", "Yesterday", "12 Oct"), absolute in the reader.

## Checklist before calling UI work done

(For an audit, report each finding as Critical = breaks WCAG or blocks a task, Warning = friction, Opportunity = improvement, with `file:line`, problem and fix.)

1. No raw colours, radii or sizes outside `theme.rs`.
2. Run the app and **look** at it in light AND dark, at 900px and 1280px wide. Compare against the bar above as a critical art director would: alignment, spacing rhythm, hierarchy, contrast, empty space, truncated text.
3. Every interactive element shows hover, pressed, focus and disabled states.
4. Keyboard only: can the whole flow be done without the mouse?
5. Empty / loading / error / long-text / many-items states checked.
6. `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.
7. Fictional data in screenshots.
8. Each group has at most one primary action; destructive actions confirm or undo; errors say what to do next; empty views offer the next step.
9. Remove one element that does not help the user (a frame, a redundant label, a second highlight), then re-render.
10. For automated renders use `egui_kittest` (`snapshot` + `wgpu` features, `Harness::builder().with_theme(..)`, `harness.render()`); if no graphics adapter is available, say the visual check was skipped.

If a screenshot looks "like egui", it is not done.
