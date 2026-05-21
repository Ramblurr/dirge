use std::io::Write;
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn new() -> std::io::Result<Self> {
        let mut stdout = std::io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        stdout.execute(Clear(ClearType::All))?;
        stdout.execute(EnableMouseCapture)?;
        // Bracketed paste lets the terminal deliver a multi-line paste as a
        // single Event::Paste, rather than a flood of keystroke events. The
        // input editor relies on this to compress long pastes into a
        // `[N lines pasted]` placeholder.
        stdout.execute(EnableBracketedPaste)?;
        // Hide the hardware cursor by default. While the agent streams output,
        // the renderer issues many MoveTo calls and the visible cursor would
        // flicker across the screen. draw_bottom re-shows it only after
        // positioning it at the input prompt.
        stdout.execute(Hide)?;
        terminal::enable_raw_mode()?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        // Send the mode-reset sequences (`DisableMouseCapture`,
        // `DisableBracketedPaste`) while we're STILL in raw mode and
        // STILL on the alt screen. Some terminals (and tmux state
        // machines) answer these resets — and other transitions like
        // leaving the alt screen — with synchronous responses (OSC 11
        // bg-color, primary DA `\x1b[?…c`, cursor-position `\x1b[…R`)
        // that travel back through stdin. If raw mode is already
        // disabled when those bytes land, the TTY line discipline
        // echoes them straight to the user's shell prompt instead of
        // letting crossterm parse and discard them.
        let _ = stdout.execute(Show);
        let _ = stdout.execute(DisableBracketedPaste);
        let _ = stdout.execute(DisableMouseCapture);
        let _ = stdout.flush();
        // Brief poll/drain pass: pull any pending terminal events
        // (including the unsolicited responses described above) from
        // the input buffer while crossterm can still parse them as
        // structured events. ~30ms is enough for local terminals to
        // flush their reply queue without making quit feel laggy; if
        // there's nothing pending the first `poll` returns `false`
        // immediately and we exit the loop.
        let deadline = std::time::Instant::now() + Duration::from_millis(30);
        while let Ok(true) = event::poll(Duration::from_millis(5)) {
            if event::read().is_err() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        let _ = terminal::disable_raw_mode();
        let _ = stdout.execute(LeaveAlternateScreen);
        let _ = stdout.flush();
    }
}
