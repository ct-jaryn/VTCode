Implement sticky thread messages in the TUI: as users scroll through long transcript history, keep the current thread/message context pinned; clicking the sticky message should jump to its exact position in the log transcript. Support loading older history beyond terminal scrollback while keeping the composer pinned and usable. Review the referenced screenshots and use the OpenAI Codex implementation as a design/behavior reference via DeepWiki MCP, then adapt the pattern cleanly to VT Code’s existing TUI architecture.

reference:

'/Users/vinhnguyenxuan/Documents/vtcode-resources/Screenshot 2026-09-30 at 14.02.35.png' '/Users/vinhnguyenxuan/Documents/vtcode-resources/Screenshot 2026-09-30 at 14.02.33.png'

"Older history loads as you scroll, beyond your terminal’s scrollback limit.

Your composer stays pinned and ready for your next message, so you can explore long sessions without scrolling back to type."
