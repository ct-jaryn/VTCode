# Shared inline message and control commands

The core and app UI protocols expose distinct `InlineCommand`, `InlineHandle`,
and `InlineSession` types. The app protocol additionally owns captured tool
output, transient palettes, and deferred bridge input. Those state and command
boundaries remain separate.

`vtcode-ui/src/tui/core_tui/types/message_commands.rs` defines the common text
payloads and their send methods once, through two crate-private macros. A macro
is used because Rust cannot insert shared fields into distinct enum variants;
replacing those variants with tuple payloads would change existing construction
and match syntax. The macros preserve both public facades, variant order, field
names/types, and handle method signatures. They add no runtime state or allocation.

| Shared variant | Payload | Handle method |
| --- | --- | --- |
| `AppendLine` | Message kind and ordered styled segments | `append_line` |
| `AppendPastedMessage` | Kind, literal text, supplied line count | `append_pasted_message` |
| `Inline` | Kind and one styled segment | `inline` |
| `ReplaceLast` | Replacement count, kind, nested segment rows, optional link rows | `replace_last`, `replace_last_with_links` |

Segment, link, and message-kind data types already have a canonical owner in
`vtcode-commons::ui_protocol`; both protocols continue using those types through
the existing UI re-exports. `SubmittedInput` also remains shared through the core
facade. No new competing payload types or generic handle abstraction are introduced.

Each protocol retains its own `send_command` implementation and state ownership.
The shared methods forward arguments unchanged: pasted line counts are not
recomputed, replacement row order and empty rows are retained, `None` links stay
distinct from `Some(empty)`, and segment styles retain their existing `Arc`.
App-only capture/review commands and transient/deferred input are not generated
by these macros.

## Common control forwarding

`types/control_commands.rs` owns 36 identical control methods in the private
`impl_inline_control_methods` macro. Both handles invoke it inside their existing
implementation. Control variant definitions remain in their respective enums,
so their original interleaving with app-only commands and variant order stay intact.

| Group | Forwarding responsibilities |
| --- | --- |
| Lifecycle | Suspend/resume, clear queue, stop/start stream, clear screen, redraw, shutdown |
| Prompt/status | Prompt, placeholder, boxed header, left/right status, activity, reasoning stage |
| Terminal title | Items, thread label, Git branch |
| Appearance | Theme, automatic color scheme, appearance config, fullscreen settings |
| Input | Cursor/input/image/vim toggles, literal text, draft restoration, suggestions |
| Agent/queue | Queued inputs, subprocess entries, subagent preview, primary agent identity/color |
| Confirmation display | Existing skip-confirmations command forwarding |

Methods were selected by comparing AST function bodies, not just names. State-aware
methods remain local: app message labels update the cached Unicode display width,
and overlay methods route through different core/app surfaces. Styled-placeholder
helpers retain their distinct visibility (public in core, private in app); the
shared public placeholder method still delegates to the local helper. Session
setters and deferred/transient ownership remain separate from handle forwarding.

The control tests check both public handles against explicit expectations for
lifecycle order, both boolean values, asymmetric optional statuses and queues,
and restored attachment/batching metadata. A separate app regression verifies
that Unicode label width and its reset still update alongside the sent command.

## Verification

```bash
cargo nextest run --locked -p vtcode-ui
```

Two focused tests exercise both handles through their public facades. They check
all four variants and both replacement methods using different kinds, segment
texts, replacement counts, pasted-line metadata, styles, and link rows. Existing
app protocol tests cover transient input, deferred queue overflow, wake-ups,
and evidence navigation. The broader UI suite covers render/layout and interaction
regressions. These automated tests do not replace a live terminal visual check.
