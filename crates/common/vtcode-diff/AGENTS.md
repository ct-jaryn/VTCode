<!-- Compact maintainer rules retain the repository instruction line budget. -->
<!-- markdownlint-disable MD013 -->
# vtcode-diff

[Root AGENTS.md](../../../AGENTS.md) | Reusable bounded diff computation and semantic preview layout.

## Key Types

`DiffDocument` + `DiffHunk`/`DiffLine` semantic model | `DiffOptions` bounds | `compute_diff`/`format_unified_diff` entry points

## Conventions

- Keep the core independent of VT Code crates and renderer themes.
- Preserve source line endings and byte-safe intraline ranges.
- All expensive diff and intraline paths require explicit bounds or deadlines.
- Layout emits semantic rows; applications supply syntax and color policy.
- Shared responsive constants keep side-by-side and unified gutter decisions consistent across renderers; measured widths are post-indent/content widths.

## Dependencies

- `similar` owns diff algorithms and deadline-aware refinement.
- `unicode-width` owns terminal display-cell measurements.
