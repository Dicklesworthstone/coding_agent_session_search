# Accessibility Guide

This document describes the accessibility features and standards for the CASS (Coding Agent Session Search) web viewer.

## Standards Compliance

CASS targets **WCAG 2.1 Level AA** compliance, which includes:

- **WCAG 2.0 Level A and AA**
- **WCAG 2.1 Level A and AA**
- Section 508 (US federal accessibility standard)
- EN 301 549 (European accessibility standard)

## Accessibility Features

### Keyboard Navigation

All functionality is accessible via keyboard:

| Key | Action |
|-----|--------|
| `Tab` | Move to next focusable element |
| `Shift+Tab` | Move to previous focusable element |
| `Enter` | Activate buttons, submit forms |
| `Space` | Toggle checkboxes, expand/collapse |
| `Escape` | Close modals, clear search |
| `/` | Focus search input (when not in text field) |
| Arrow keys | Navigate within menus and lists |

**Skip Links**: A "Skip to main content" link appears when pressing Tab, allowing keyboard users to bypass navigation.

### Screen Reader Support

#### ARIA Labels and Roles

- All interactive elements have accessible names via `aria-label` or associated `<label>`
- Main content areas use ARIA landmarks (`role="main"`, `role="navigation"`)
- Dynamic content updates use `aria-live` regions
- Progress indicators use `role="progressbar"` with proper value attributes

#### Document Structure

- Single `<h1>` heading per page
- Logical heading hierarchy (no skipped levels)
- All images have `alt` attributes
- Form fields have associated labels

#### Live Regions

```html
<!-- Error announcements (assertive) -->
<div id="auth-error" role="alert" aria-live="assertive"></div>

<!-- Status updates (polite) -->
<div id="sr-announcer" aria-live="polite" aria-atomic="true"></div>
```

### Color Contrast

All text meets WCAG AA contrast requirements:

| Element | Contrast Ratio | Requirement |
|---------|---------------|-------------|
| Body text | > 12:1 | 4.5:1 (normal text) |
| Muted text | > 5:1 | 4.5:1 (normal text) |
| Large headings | > 8:1 | 3:1 (large text) |
| Button text | > 4.5:1 | 4.5:1 (normal text) |

**Themes**: Both light and dark themes are designed with accessibility in mind.

### Focus Indicators

All focusable elements have visible focus indicators:

```css
:focus {
    outline: 2px solid var(--color-primary);
    outline-offset: 2px;
}

:focus-visible {
    outline: 3px solid var(--color-primary);
    box-shadow: 0 0 0 6px rgba(59, 130, 246, 0.25);
}
```

### Reduced Motion

Users who prefer reduced motion (via `prefers-reduced-motion: reduce`) will experience:

- No animations or transitions
- Instant state changes
- Static loading indicators

```css
@media (prefers-reduced-motion: reduce) {
    *, *::before, *::after {
        animation-duration: 0.01ms !important;
        animation-iteration-count: 1 !important;
        transition-duration: 0.01ms !important;
    }
}
```

## Testing

### Automated Testing

We use multiple tools for accessibility testing:

1. **axe-core** (via Playwright): WCAG 2.1 AA compliance
2. **Rust HTML Auditor**: Static HTML analysis for structure
3. **Lighthouse**: Overall accessibility scoring

Run accessibility tests:

```bash
# Playwright axe-core tests
cd tests && npm run test:e2e

# Rust accessibility tests
cargo test --test pages_accessibility_e2e
```

### Manual Testing Checklist

#### Keyboard Testing
- [ ] Can navigate to all interactive elements with Tab
- [ ] Tab order follows visual layout
- [ ] Can activate buttons/links with Enter/Space
- [ ] Can escape from modals with Escape
- [ ] Focus is visible on all elements
- [ ] Focus is not trapped unexpectedly

#### Screen Reader Testing (VoiceOver/NVDA)
- [ ] Page title is announced
- [ ] Headings are properly structured (h1, h2, h3)
- [ ] All images have alt text
- [ ] Form fields are properly labeled
- [ ] Buttons have descriptive names
- [ ] Dynamic updates are announced
- [ ] Error messages are announced

#### Visual Testing
- [ ] Text is readable at 200% zoom
- [ ] No horizontal scroll at 200% zoom
- [ ] Color contrast meets WCAG AA
- [ ] Information not conveyed by color alone
- [ ] Focus indicators are visible
- [ ] Works with Windows High Contrast mode

#### Motion Testing
- [ ] Respects prefers-reduced-motion
- [ ] No flashing content (>3 flashes/second)
- [ ] Animations can be paused

### Screen Reader Testing Guide

#### VoiceOver (macOS)

1. Open Safari, navigate to the archive viewer
2. Press `Cmd+F5` to enable VoiceOver
3. Press `VO+Right` (Ctrl+Option+Right) to navigate

Expected announcements:
- "CASS Archive Viewer, web content"
- "Heading level 1, Unlock Archive"
- "Password, secure text field"
- "Unlock, button"

#### NVDA (Windows)

1. Open Chrome/Firefox
2. Press `Insert+Q` to start NVDA
3. Press `Tab` to navigate through elements

## Known Issues

None currently documented.

## Reporting Issues

If you encounter accessibility barriers:

1. File an issue at https://github.com/anthropics/cass/issues
2. Include:
   - Browser and version
   - Assistive technology used
   - Steps to reproduce
   - Expected vs actual behavior

## Resources

- [WCAG 2.1 Quick Reference](https://www.w3.org/WAI/WCAG21/quickref/)
- [axe-core Rules](https://dequeuniversity.com/rules/axe/4.10)
- [WebAIM Contrast Checker](https://webaim.org/resources/contrastchecker/)
- [ARIA Authoring Practices](https://www.w3.org/WAI/ARIA/apg/)
