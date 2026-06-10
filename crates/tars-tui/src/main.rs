//! tars-tui — a Codex-look coding TUI driven by the TARS runtime (Doc 22).
//!
//! M0 foundation probe: stand up a ratatui terminal against **upstream**
//! ratatui 0.29 / crossterm 0.28 (not Codex's nornagon forks — we de-forked
//! for supply-chain trust) and draw one frame, proving the dependency
//! foundation builds before Codex's view layer is vendored + adapted.

use std::io::stdout;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    loop {
        terminal.draw(|f| {
            let block = Block::default()
                .title(" tars-tui (M0 probe) ")
                .borders(Borders::ALL);
            let body =
                Paragraph::new("Codex view layer not vendored yet — press q to quit.").block(block);
            f.render_widget(body, f.area());
        })?;

        if let Event::Key(k) = event::read()? {
            if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                break;
            }
        }
    }

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
