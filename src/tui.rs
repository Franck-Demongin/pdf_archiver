use crate::{Cli, parse_pages, run};
use anyhow::Result;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Focus {
    #[default]
    Browser,
    Entries,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    EditingSelector(usize),
    EditingOutput,
    ConfirmOverwrite,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Normal
    }
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    selector: String,
}

#[derive(Default)]
struct App {
    focus: Focus,
    mode: Mode,
    cwd: PathBuf,
    browser_entries: Vec<PathBuf>,
    browser_state: ListState,
    entries: Vec<Entry>,
    entry_state: ListState,
    input_buffer: String,
    output: String,
    status: String,
    force: bool,
    quit: bool,
    run_requested: bool,
}

impl App {
    fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut app = Self {
            cwd,
            output: "output.pdf".to_string(),
            status: "Prêt. Tab: changer de panneau, ↑/↓: naviguer, Entrée: ajouter/sélectionner, q: quitter".to_string(),
            ..Self::default()
        };
        app.refresh_browser();
        app
    }

    fn refresh_browser(&mut self) {
        self.browser_entries.clear();
        if let Some(parent) = self.cwd.parent() {
            self.browser_entries.push(parent.to_path_buf());
        }
        if let Ok(read) = fs::read_dir(&self.cwd) {
            let mut dirs = Vec::new();
            let mut files = Vec::new();
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("pdf"))
                    .unwrap_or(false)
                {
                    files.push(path);
                }
            }
            dirs.sort();
            files.sort();
            self.browser_entries.extend(dirs);
            self.browser_entries.extend(files);
        }
        if self.browser_state.selected().is_none() && !self.browser_entries.is_empty() {
            self.browser_state.select(Some(0));
        }
    }

    fn current_browser_path(&self) -> Option<PathBuf> {
        self.browser_state
            .selected()
            .and_then(|i| self.browser_entries.get(i).cloned())
    }
}

use std::fs;

pub fn run_tui() -> Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new();
    let result = run_app(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn run_app(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        if app.run_requested {
            app.run_requested = false;
            execute_run(app);
        }
        terminal.draw(|frame| render(frame, app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(app, key.code);
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyCode) {
    match app.mode {
        Mode::EditingSelector(idx) => match key {
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.input_buffer.clear();
            }
            KeyCode::Enter => {
                if let Some(entry) = app.entries.get_mut(idx) {
                    entry.selector = app.input_buffer.trim().to_string();
                }
                app.mode = Mode::Normal;
                app.input_buffer.clear();
            }
            KeyCode::Backspace => {
                app.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                app.input_buffer.push(c);
            }
            _ => {}
        },
        Mode::EditingOutput => match key {
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.input_buffer.clear();
            }
            KeyCode::Enter => {
                if !app.input_buffer.trim().is_empty() {
                    app.output = app.input_buffer.trim().to_string();
                }
                app.mode = Mode::Normal;
                app.input_buffer.clear();
            }
            KeyCode::Backspace => {
                app.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                app.input_buffer.push(c);
            }
            _ => {}
        },
        Mode::ConfirmOverwrite => match key {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('o') | KeyCode::Char('O') => {
                app.mode = Mode::Normal;
                app.run_requested = true;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.status = "Annulé.".to_string();
            }
            _ => {}
        },
        Mode::Normal => match key {
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Focus::Browser => Focus::Entries,
                    Focus::Entries => Focus::Browser,
                };
            }
            KeyCode::Up => match app.focus {
                Focus::Browser => {
                    if let Some(i) = app.browser_state.selected() {
                        if i > 0 {
                            app.browser_state.select(Some(i - 1));
                        }
                    }
                }
                Focus::Entries => {
                    if let Some(i) = app.entry_state.selected() {
                        if i > 0 {
                            app.entry_state.select(Some(i - 1));
                        }
                    }
                }
            },
            KeyCode::Down => match app.focus {
                Focus::Browser => {
                    if let Some(i) = app.browser_state.selected() {
                        if i + 1 < app.browser_entries.len() {
                            app.browser_state.select(Some(i + 1));
                        }
                    } else if !app.browser_entries.is_empty() {
                        app.browser_state.select(Some(0));
                    }
                }
                Focus::Entries => {
                    if let Some(i) = app.entry_state.selected() {
                        if i + 1 < app.entries.len() {
                            app.entry_state.select(Some(i + 1));
                        }
                    } else if !app.entries.is_empty() {
                        app.entry_state.select(Some(0));
                    }
                }
            },
            KeyCode::Enter => match app.focus {
                Focus::Browser => {
                    if let Some(path) = app.current_browser_path() {
                        if path.is_dir() {
                            app.cwd = path;
                            app.browser_state = ListState::default();
                            app.refresh_browser();
                        } else {
                            app.entries.push(Entry {
                                path,
                                selector: String::new(),
                            });
                            app.entry_state.select(Some(app.entries.len() - 1));
                            app.status = "PDF ajouté. 'e' pour définir les pages.".to_string();
                        }
                    }
                }
                Focus::Entries => {
                    if let Some(i) = app.entry_state.selected() {
                        app.mode = Mode::EditingSelector(i);
                        app.input_buffer = app
                            .entries
                            .get(i)
                            .map(|e| e.selector.clone())
                            .unwrap_or_default();
                    }
                }
            },
            KeyCode::Char('e') => {
                if let Some(i) = app.entry_state.selected() {
                    app.mode = Mode::EditingSelector(i);
                    app.input_buffer = app
                        .entries
                        .get(i)
                        .map(|e| e.selector.clone())
                        .unwrap_or_default();
                }
            }
            KeyCode::Char('d') => {
                if let Some(i) = app.entry_state.selected() {
                    if i < app.entries.len() {
                        app.entries.remove(i);
                        if app.entries.is_empty() {
                            app.entry_state.select(None);
                        } else {
                            app.entry_state.select(Some(i.min(app.entries.len() - 1)));
                        }
                        app.status = "Entrée supprimée.".to_string();
                    }
                }
            }
            KeyCode::Char('o') => {
                app.mode = Mode::EditingOutput;
                app.input_buffer = app.output.clone();
            }
            KeyCode::Char('f') => {
                app.force = !app.force;
                app.status = format!(
                    "Écrasement forcé : {}",
                    if app.force { "oui" } else { "non" }
                );
            }
            KeyCode::Char('r') => {
                if app.entries.is_empty() {
                    app.status = "Aucune entrée.".to_string();
                    return;
                }
                if app.output.trim().is_empty() {
                    app.status = "Chemin de sortie vide.".to_string();
                    return;
                }
                let out = Path::new(&app.output);
                if out.exists() && !app.force {
                    app.mode = Mode::ConfirmOverwrite;
                    app.status = format!("Le fichier {} existe. Écraser ? (y/n)", out.display());
                } else {
                    app.run_requested = true;
                }
            }
            _ => {}
        },
    }
}

fn execute_run(app: &mut App) {
    let specs: Vec<String> = app
        .entries
        .iter()
        .map(|e| {
            if e.selector.trim().is_empty() {
                e.path.display().to_string()
            } else {
                format!("{}:{}", e.path.display(), e.selector.trim())
            }
        })
        .collect();

    for (entry, spec) in app.entries.iter().zip(specs.iter()) {
        let (_, sel) = crate::split_input(spec);
        if let Some(s) = sel {
            let total = match lopdf_total_pages(&entry.path) {
                Ok(t) => t,
                Err(err) => {
                    app.status = format!("Erreur lecture {}: {}", entry.path.display(), err);
                    return;
                }
            };
            if let Err(err) = parse_pages(&s, total) {
                app.status = format!("Sélecteur invalide pour {} : {}", entry.path.display(), err);
                return;
            }
        }
    }

    let cli = Cli {
        inputs: specs,
        output: Some(PathBuf::from(&app.output)),
        force: app.force,
        tui: false,
    };
    match run(cli) {
        Ok(()) => {
            app.status = format!("Écrit {}", app.output);
        }
        Err(err) => {
            app.status = format!("Erreur : {err}");
        }
    }
}

fn lopdf_total_pages(path: &Path) -> Result<u32, anyhow::Error> {
    let doc = lopdf::Document::load(path)?;
    Ok(doc.get_pages().len() as u32)
}

fn render(frame: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(4)])
        .split(frame.area());

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(outer[0]);

    let browser_items: Vec<ListItem> = app
        .browser_entries
        .iter()
        .map(|p| {
            let name = if p.is_dir() {
                format!("{}/", p.file_name().unwrap_or_default().to_string_lossy())
            } else {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            };
            ListItem::new(Line::from(Span::styled(name, Style::default())))
        })
        .collect();

    let browser_title = format!("Fichiers — {}", app.cwd.display());
    let browser_list = List::new(browser_items)
        .block(
            Block::default()
                .title(browser_title)
                .borders(Borders::ALL)
                .border_style(if app.focus == Focus::Browser {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                }),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(browser_list, main[0], &mut app.browser_state);

    let entry_items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|e| {
            let sel = if e.selector.trim().is_empty() {
                "toutes".to_string()
            } else {
                e.selector.clone()
            };
            ListItem::new(Line::from(format!("{}  [{}] ", e.path.display(), sel)))
        })
        .collect();
    let entries_list = List::new(entry_items)
        .block(
            Block::default()
                .title("Entrées (pages)")
                .borders(Borders::ALL)
                .border_style(if app.focus == Focus::Entries {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                }),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(entries_list, main[1], &mut app.entry_state);

    let footer_lines = vec![
        Line::from(format!(
            "Sortie : {}   (o: modifier, f: forcer={})",
            app.output, app.force
        )),
        Line::from(app.status.clone()),
        Line::from(
            "Tab: panneau  ↑/↓: nav  Entrée: ajouter/éditer  e: pages  d: suppr  r: générer  q: quitter",
        ),
    ];
    let footer = Paragraph::new(footer_lines).wrap(Wrap { trim: true });
    frame.render_widget(footer, outer[1]);

    match app.mode {
        Mode::EditingSelector(_) | Mode::EditingOutput => {
            let area = centered_rect(60, 20, frame.area());
            frame.render_widget(Clear, area);
            let title = match app.mode {
                Mode::EditingSelector(_) => "Pages à conserver (ex. 1,3-5,10-z)",
                Mode::EditingOutput => "Chemin de sortie",
                _ => "",
            };
            let block = Block::default().title(title).borders(Borders::ALL);
            let para = Paragraph::new(app.input_buffer.clone()).block(block);
            frame.render_widget(para, area);
        }
        Mode::ConfirmOverwrite => {
            let area = centered_rect(50, 15, frame.area());
            frame.render_widget(Clear, area);
            let block = Block::default().title("Confirmation").borders(Borders::ALL);
            let para = Paragraph::new(format!("{}\n\nÉcraser ? (y/n)", app.status)).block(block);
            frame.render_widget(para, area);
        }
        Mode::Normal => {}
    }
}

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    r: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
