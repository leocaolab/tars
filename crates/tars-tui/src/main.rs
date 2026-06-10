//! tars-tui — a Codex-look coding TUI driven by the TARS runtime (Doc 22).
//!
//! M0 foundation probe: stand up a ratatui terminal against **upstream**
//! ratatui 0.29 / crossterm 0.28 (not Codex's nornagon forks — we de-forked
//! for supply-chain trust) and draw one frame, proving the dependency
//! foundation builds before Codex's view layer is vendored + adapted.

#[allow(clippy::all, dead_code)]
mod view;

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

    const SAMPLE_MD: &str = "\
# tars-tui

Codex's markdown renderer, **vendored** and running on *upstream* ratatui.

- bullet one
- bullet two with `inline code`

```rust
fn hello() { println!(\"from TARS\"); }
```

Press **q** to quit.";

    loop {
        terminal.draw(|f| {
            let area = f.area();
            // Render markdown through the vendored Codex pipeline — this is
            // "the look" the whole port is for.
            let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
            view::markdown::append_markdown(
                SAMPLE_MD,
                Some(area.width.saturating_sub(2) as usize),
                &mut lines,
            );
            let block = Block::default()
                .title(" tars-tui (M0a — Codex renderer on upstream ratatui) ")
                .borders(Borders::ALL);
            let body = Paragraph::new(ratatui::text::Text::from(lines)).block(block);
            f.render_widget(body, area);
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
