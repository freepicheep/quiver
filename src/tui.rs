use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Tabs, Wrap,
    },
};

use crate::error::{QuiverError, Result};
use crate::lockfile::{LockedPackageKind, Lockfile};
use crate::manifest::Manifest;

const BUILTIN_PLUGINS: &[&str] = &[
    "nu_plugin_custom_values",
    "nu_plugin_example",
    "nu_plugin_formats",
    "nu_plugin_gstat",
    "nu_plugin_inc",
    "nu_plugin_polars",
    "nu_plugin_query",
    "nu_plugin_stress_internals",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    Install,
    Update,
    Remove { name: String },
    AddModule { url: String },
    AddPlugin { url: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dependencies,
    Graph,
    Search,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    AddUrl,
    ConfirmRemove { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DependencyKind {
    Module,
    Plugin,
    Transitive,
}

#[derive(Debug, Clone)]
struct DependencyRow {
    name: String,
    kind: DependencyKind,
    git: String,
    requested: String,
    locked_rev: Option<String>,
    locked_tag: Option<String>,
    checksum: Option<String>,
}

struct App {
    project_dir: Option<PathBuf>,
    package_name: String,
    package_version: String,
    rows: Vec<DependencyRow>,
    tab: Tab,
    selected: usize,
    search_selected: usize,
    readme_scroll: u16,
    detail_readme_scroll: u16,
    graph_scroll: u16,
    local_readme: String,
    local_license: String,
    status: String,
    input: String,
    readme_url: String,
    readme: String,
    input_mode: InputMode,
    action: Option<TuiAction>,
    quit: bool,
}

pub fn run(cwd: &Path) -> Result<Option<TuiAction>> {
    if !io::stderr().is_terminal() {
        return Err(QuiverError::Other(
            "qv without a subcommand opens the TUI; run it in an interactive terminal".to_string(),
        ));
    }

    let mut app = App::load(cwd)?;

    enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let tui_result = (|| -> Result<Option<TuiAction>> {
        loop {
            terminal.draw(|frame| render(frame, &mut app))?;

            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    handle_key(&mut app, key.code, key.modifiers);
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown if app.tab == Tab::Dependencies => {
                        app.detail_readme_scroll = app.detail_readme_scroll.saturating_add(3);
                    }
                    MouseEventKind::ScrollUp if app.tab == Tab::Dependencies => {
                        app.detail_readme_scroll = app.detail_readme_scroll.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown if app.tab == Tab::Search => {
                        app.readme_scroll = app.readme_scroll.saturating_add(3);
                    }
                    MouseEventKind::ScrollUp if app.tab == Tab::Search => {
                        app.readme_scroll = app.readme_scroll.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown if app.tab == Tab::Graph => {
                        app.graph_scroll = app.graph_scroll.saturating_add(3);
                    }
                    MouseEventKind::ScrollUp if app.tab == Tab::Graph => {
                        app.graph_scroll = app.graph_scroll.saturating_sub(3);
                    }
                    _ => {}
                },
                _ => continue,
            }
            if app.quit || app.action.is_some() {
                break;
            }
        }

        Ok(app.action.take())
    })();

    let cleanup_result = (|| -> io::Result<()> {
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;
        Ok(())
    })();

    match (tui_result, cleanup_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(err)) | (Err(_), Err(err)) => Err(err.into()),
    }
}

impl App {
    fn load(cwd: &Path) -> Result<Self> {
        let Some(project_dir) = Manifest::find_project_dir(cwd) else {
            return Ok(Self {
                project_dir: None,
                package_name: "No quiver project".to_string(),
                package_version: String::new(),
                rows: Vec::new(),
                tab: Tab::Dependencies,
                selected: 0,
                search_selected: 0,
                readme_scroll: 0,
                detail_readme_scroll: 0,
                graph_scroll: 0,
                local_readme: String::new(),
                local_license: String::new(),
                status: "Run qv init to create nupackage.toml, or q to quit.".to_string(),
                input: String::new(),
                readme_url: String::new(),
                readme: "No nupackage.toml was found in this directory or its parents.".to_string(),
                input_mode: InputMode::Normal,
                action: None,
                quit: false,
            });
        };

        let manifest = Manifest::from_dir(&project_dir)?;
        let lockfile = read_lockfile(&project_dir)?;
        let rows = dependency_rows(&manifest, lockfile.as_ref());
        let status = if rows.is_empty() {
            "No dependencies yet. Press a to paste a GitHub URL.".to_string()
        } else {
            "Use up/down to inspect dependencies; g opens the graph; a adds from GitHub."
                .to_string()
        };

        let (local_readme, local_license) = rows
            .first()
            .map(|row| load_dist_info_for_row(&project_dir, row))
            .unwrap_or_default();

        Ok(Self {
            project_dir: Some(project_dir),
            package_name: manifest.package.name,
            package_version: manifest.package.version,
            rows,
            tab: Tab::Dependencies,
            selected: 0,
            search_selected: 0,
            readme_scroll: 0,
            detail_readme_scroll: 0,
            graph_scroll: 0,
            local_readme,
            local_license,
            status,
            input: String::new(),
            readme_url: String::new(),
            readme: "Paste a GitHub repository URL, then press Enter to preview its README."
                .to_string(),
            input_mode: InputMode::Normal,
            action: None,
            quit: false,
        })
    }

    fn selected_row(&self) -> Option<&DependencyRow> {
        self.rows.get(self.selected)
    }

    fn can_manage(&self) -> bool {
        self.project_dir.is_some()
    }
}

fn read_lockfile(project_dir: &Path) -> Result<Option<Lockfile>> {
    let path = project_dir.join("quiver.lock");
    if path.exists() {
        Ok(Some(Lockfile::from_path(&path)?))
    } else {
        Ok(None)
    }
}

fn dependency_rows(manifest: &Manifest, lockfile: Option<&Lockfile>) -> Vec<DependencyRow> {
    let mut locked_by_name = HashMap::new();
    if let Some(lockfile) = lockfile {
        for package in &lockfile.packages {
            locked_by_name.insert((package.name.clone(), package.kind.clone()), package);
        }
    }

    let mut rows = Vec::new();
    let mut direct_names = HashSet::new();

    let mut modules: Vec<_> = manifest.dependencies.modules.iter().collect();
    modules.sort_by(|a, b| a.0.cmp(b.0));
    for (name, spec) in modules {
        direct_names.insert((name.clone(), LockedPackageKind::Module));
        let locked = locked_by_name.get(&(name.clone(), LockedPackageKind::Module));
        rows.push(DependencyRow {
            name: name.clone(),
            kind: DependencyKind::Module,
            git: spec.git.clone(),
            requested: requested_ref(&spec.tag, &spec.rev, &spec.branch),
            locked_rev: locked.map(|p| p.rev.clone()),
            locked_tag: locked.and_then(|p| p.tag.clone()),
            checksum: locked.map(|p| p.sha256.clone()),
        });
    }

    let mut plugins: Vec<_> = manifest.dependencies.plugins.iter().collect();
    plugins.sort_by(|a, b| a.0.cmp(b.0));
    for (name, spec) in plugins {
        direct_names.insert((name.clone(), LockedPackageKind::Plugin));
        let locked = locked_by_name.get(&(name.clone(), LockedPackageKind::Plugin));
        rows.push(DependencyRow {
            name: name.clone(),
            kind: DependencyKind::Plugin,
            git: if spec.git.is_empty() {
                spec.source.as_deref().unwrap_or("nu-core").to_string()
            } else {
                spec.git.clone()
            },
            requested: requested_ref(&spec.tag, &spec.rev, &spec.branch),
            locked_rev: locked.map(|p| p.rev.clone()),
            locked_tag: locked.and_then(|p| p.tag.clone()),
            checksum: locked.map(|p| p.sha256.clone()),
        });
    }

    if let Some(lockfile) = lockfile {
        let mut transitive: Vec<_> = lockfile
            .packages
            .iter()
            .filter(|package| {
                package.kind == LockedPackageKind::Module
                    && !direct_names.contains(&(package.name.clone(), package.kind.clone()))
            })
            .collect();
        transitive.sort_by(|a, b| a.name.cmp(&b.name));
        for package in transitive {
            rows.push(DependencyRow {
                name: package.name.clone(),
                kind: DependencyKind::Transitive,
                git: package.git.clone(),
                requested: package.tag.clone().unwrap_or_else(|| "locked".to_string()),
                locked_rev: Some(package.rev.clone()),
                locked_tag: package.tag.clone(),
                checksum: Some(package.sha256.clone()),
            });
        }
    }

    rows
}

fn requested_ref(tag: &Option<String>, rev: &Option<String>, branch: &Option<String>) -> String {
    if let Some(tag) = tag {
        format!("tag {tag}")
    } else if let Some(rev) = rev {
        format!("rev {}", short_rev(rev))
    } else if let Some(branch) = branch {
        format!("branch {branch}")
    } else {
        "none".to_string()
    }
}

fn render(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = frame.area();
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);

    render_header(frame, app, shell[0]);
    match app.tab {
        Tab::Dependencies => render_dependencies(frame, app, shell[1]),
        Tab::Graph => render_graph(frame, app, shell[1]),
        Tab::Search => render_search(frame, app, shell[1]),
    }
    render_footer(frame, app, shell[2]);

    match &app.input_mode {
        InputMode::AddUrl => render_input(frame, app, "Paste GitHub repository URL"),
        InputMode::ConfirmRemove { name } => {
            render_confirm(frame, &format!("Remove '{name}' from nupackage.toml? y/N"))
        }
        InputMode::Normal => {}
    }
}

fn render_header(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let titles = ["Dependencies", "Graph", "Add"]
        .iter()
        .map(|title| Line::from(Span::styled(*title, Style::default().fg(Color::Cyan))))
        .collect::<Vec<_>>();
    let selected = match app.tab {
        Tab::Dependencies => 0,
        Tab::Graph => 1,
        Tab::Search => 2,
    };
    let title = if app.package_version.is_empty() {
        app.package_name.clone()
    } else {
        format!("{} {}", app.package_name, app.package_version)
    };
    let tabs = Tabs::new(titles)
        .select(selected)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, area);
}

fn render_dependencies(frame: &mut ratatui::Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);

    let items = if app.rows.is_empty() {
        vec![ListItem::new("No dependencies")]
    } else {
        app.rows
            .iter()
            .map(|row| {
                let status = if row.locked_rev.is_some() {
                    "locked"
                } else {
                    "missing"
                };
                ListItem::new(Line::from(vec![
                    Span::styled(kind_label(&row.kind), kind_style(&row.kind)),
                    Span::raw(" "),
                    Span::styled(&row.name, Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(format!("  {status}")),
                ]))
            })
            .collect()
    };
    let list = List::new(items)
        .block(Block::default().title("Dependencies").borders(Borders::ALL))
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let mut list_state = ListState::default();
    if !app.rows.is_empty() {
        list_state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, chunks[0], &mut list_state);

    // Right pane: split into details (top) and README (bottom)
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(4)])
        .split(chunks[1]);

    let detail = selected_detail(app);
    frame.render_widget(
        Paragraph::new(detail)
            .block(Block::default().title("Details").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        right_chunks[0],
    );

    let readme_content = if app.local_readme.is_empty() {
        if app.selected_row().is_some_and(|r| r.locked_rev.is_some()) {
            "No README found in dist-info."
        } else {
            "Not installed."
        }
    } else {
        app.local_readme.as_str()
    };

    let readme_paragraph = Paragraph::new(tui_markdown::from_str(readme_content))
        .block(Block::default().title("README").borders(Borders::ALL))
        .scroll((app.detail_readme_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(readme_paragraph, right_chunks[1]);

    let content_height = app.local_readme.lines().count().saturating_sub(1) as u16;
    let mut scrollbar_state =
        ScrollbarState::new(content_height as usize).position(app.detail_readme_scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        right_chunks[1],
        &mut scrollbar_state,
    );
}

fn selected_detail(app: &App) -> String {
    let Some(row) = app.selected_row() else {
        return "Press a to paste a GitHub repository URL and preview its README.".to_string();
    };

    let mut detail = String::new();
    detail.push_str(&format!("name: {}\n", row.name));
    detail.push_str(&format!("kind: {}\n", kind_label(&row.kind)));
    if !app.local_license.is_empty() {
        detail.push_str(&format!("license: {}\n", app.local_license));
    }
    detail.push_str(&format!("source: {}\n", row.git));
    detail.push_str(&format!("requested: {}\n", row.requested));
    if let Some(rev) = &row.locked_rev {
        detail.push_str(&format!("locked rev: {}\n", rev));
    } else {
        detail.push_str("locked rev: not installed\n");
    }
    if let Some(checksum) = &row.checksum {
        detail.push_str(&format!("sha256: {}\n", checksum));
    }
    detail
}

struct Canvas {
    grid: Vec<Vec<char>>,
    width: usize,
    height: usize,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            grid: vec![vec![' '; width]; height],
            width,
            height,
        }
    }

    fn write(&mut self, x: usize, y: usize, s: &str) {
        if y >= self.height {
            return;
        }
        for (i, c) in s.chars().enumerate() {
            if x + i < self.width {
                self.grid[y][x + i] = c;
            }
        }
    }

    fn to_string(&self) -> String {
        self.grid
            .iter()
            .map(|row| row.iter().collect::<String>())
            .map(|s| s.trim_end().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn draw_root_box(
    canvas: &mut Canvas,
    x: usize,
    y: usize,
    name: &str,
    has_children: bool,
) -> (usize, usize) {
    let name_len = name.chars().count();
    let w = name_len.max(10) + 4;
    let top = format!("┌{}┐", "─".repeat(w));
    let empty = format!("│{}│", " ".repeat(w));

    let pad_left = (w - name_len) / 2;
    let pad_right = w - name_len - pad_left;
    let text = format!(
        "│{}{}{}│",
        " ".repeat(pad_left),
        name,
        " ".repeat(pad_right)
    );

    let mid = w / 2;
    let bottom = if has_children {
        format!("└{}┬{}┘", "─".repeat(mid), "─".repeat(w - mid - 1))
    } else {
        format!("└{}┘", "─".repeat(w))
    };

    canvas.write(x, y, &top);
    canvas.write(x, y + 1, &empty);
    canvas.write(x, y + 2, &text);
    canvas.write(x, y + 3, &empty);
    canvas.write(x, y + 4, &bottom);

    (x + mid + 1, y + 4)
}

fn draw_dep_box(
    canvas: &mut Canvas,
    x: usize,
    y: usize,
    kind: &str,
    version: &str,
    name: &str,
) -> (usize, usize) {
    let kind_len = kind.chars().count();
    let ver_len = version.chars().count();
    let name_len = name.chars().count();

    let w1 = kind_len.max(ver_len) + 2;
    let w2 = name_len + 2;

    let top = format!("┌{}┬{}┐", "─".repeat(w1), "─".repeat(w2));

    let pad_kind_l = (w1 - kind_len) / 2;
    let pad_kind_r = w1 - kind_len - pad_kind_l;
    let line1 = format!(
        "│{}{}{}│{}│",
        " ".repeat(pad_kind_l),
        kind,
        " ".repeat(pad_kind_r),
        " ".repeat(w2)
    );

    let line2 = format!(
        "├{}┤ {}{}│",
        "─".repeat(w1),
        name,
        " ".repeat(w2 - name_len - 1)
    );

    let pad_ver_l = (w1 - ver_len) / 2;
    let pad_ver_r = w1 - ver_len - pad_ver_l;
    let line3 = format!(
        "│{}{}{}│{}│",
        " ".repeat(pad_ver_l),
        version,
        " ".repeat(pad_ver_r),
        " ".repeat(w2)
    );

    let bottom = format!("└{}┴{}┘", "─".repeat(w1), "─".repeat(w2));

    canvas.write(x, y, &top);
    canvas.write(x, y + 1, &line1);
    canvas.write(x, y + 2, &line2);
    canvas.write(x, y + 3, &line3);
    canvas.write(x, y + 4, &bottom);

    (x, y + 2)
}

fn render_graph(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let mut deps = Vec::new();

    let direct: Vec<_> = app
        .rows
        .iter()
        .filter(|row| !matches!(row.kind, DependencyKind::Transitive))
        .collect();
    deps.extend(direct);

    let transitive: Vec<_> = app
        .rows
        .iter()
        .filter(|row| matches!(row.kind, DependencyKind::Transitive))
        .collect();
    deps.extend(transitive);

    let height = 5 + (deps.len().max(1)) * 6 + 2;
    let width = 200;
    let mut canvas = Canvas::new(width, height);

    let package_name = if app.package_name.is_empty() {
        "No Project"
    } else {
        &app.package_name
    };
    let (root_px, root_py) = draw_root_box(&mut canvas, 4, 1, package_name, !deps.is_empty());

    let spine_x = root_px;
    let mut current_y = root_py;

    for (i, row) in deps.iter().enumerate() {
        let is_last = i == deps.len() - 1;
        let dep_y = 6 + i * 6 + 1;
        let dep_x = spine_x + 8;

        let version = if let Some(tag) = &row.locked_tag {
            tag.clone()
        } else if let Some(rev) = &row.locked_rev {
            short_rev(rev).to_string()
        } else {
            row.requested.clone()
        };

        let (in_x, in_y) = draw_dep_box(
            &mut canvas,
            dep_x,
            dep_y,
            kind_label(&row.kind),
            &version,
            &row.name,
        );

        for y in (current_y + 1)..=in_y {
            let ch = if y == in_y {
                if is_last { '└' } else { '├' }
            } else {
                '│'
            };
            canvas.write(spine_x, y, &ch.to_string());
        }

        for x in (spine_x + 1)..(in_x - 1) {
            canvas.write(x, in_y, "─");
        }

        canvas.write(in_x - 1, in_y, "►");

        current_y = in_y;
    }

    let text = canvas.to_string();

    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .title("Dependency Graph")
                    .borders(Borders::ALL),
            )
            .scroll((app.graph_scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_search(frame: &mut ratatui::Frame<'_>, app: &mut App, area: Rect) {
    let horizontal_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(horizontal_chunks[0]);

    let url = if app.readme_url.is_empty() {
        "No repository loaded".to_string()
    } else {
        app.readme_url.clone()
    };
    frame.render_widget(
        Paragraph::new(url).block(
            Block::default()
                .title("GitHub Repository")
                .borders(Borders::ALL),
        ),
        left_chunks[0],
    );

    let paragraph = Paragraph::new(tui_markdown::from_str(app.readme.as_str()))
        .block(Block::default().title("README").borders(Borders::ALL))
        .scroll((app.readme_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, left_chunks[1]);

    let content_height = app.readme.lines().count().saturating_sub(1) as u16;
    let mut scrollbar_state =
        ScrollbarState::new(content_height as usize).position(app.readme_scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        left_chunks[1],
        &mut scrollbar_state,
    );

    let items: Vec<ListItem> = BUILTIN_PLUGINS
        .iter()
        .map(|p| ListItem::new(Line::from(p.to_string())))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .title("Built-in Plugins")
                .borders(Borders::ALL),
        )
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let mut list_state = ListState::default();
    list_state.select(Some(app.search_selected));
    frame.render_stateful_widget(list, horizontal_chunks[1], &mut list_state);
}

fn render_footer(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let keys = match app.tab {
        Tab::Dependencies => {
            "q quit | tab switch | j/k select | PgUp/PgDn scroll | i install | u update | r remove | a add"
        }
        Tab::Graph => "q quit | tab switch | d dependencies | a add URL",
        Tab::Search => {
            "q quit | tab switch | j/k select plugin | Enter add plugin | a paste URL | m add repo module | p add repo plugin | PgUp/PgDn scroll README"
        }
    };
    let text = format!("{keys}\n{}", app.status);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_input(frame: &mut ratatui::Frame<'_>, app: &App, title: &str) {
    let area = centered_rect(70, 20, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(app.input.as_str())
            .block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn render_confirm(frame: &mut ratatui::Frame<'_>, message: &str) {
    let area = centered_rect(60, 20, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .block(Block::default().title("Confirm").borders(Borders::ALL)),
        area,
    );
}

fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    match app.input_mode.clone() {
        InputMode::Normal => handle_normal_key(app, code, modifiers),
        InputMode::AddUrl => handle_url_key(app, code),
        InputMode::ConfirmRemove { name } => handle_remove_confirm_key(app, code, name),
    }
}

fn handle_normal_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Tab | KeyCode::Right => app.tab = next_tab(app.tab),
        KeyCode::BackTab | KeyCode::Left => app.tab = previous_tab(app.tab),
        KeyCode::Char('d') => app.tab = Tab::Dependencies,
        KeyCode::Char('g') => app.tab = Tab::Graph,
        KeyCode::Char('s') => app.tab = Tab::Search,
        KeyCode::Down | KeyCode::Char('j') => {
            if app.tab == Tab::Search {
                if app.search_selected + 1 < BUILTIN_PLUGINS.len() {
                    app.search_selected += 1;
                }
            } else if app.tab == Tab::Graph {
                app.graph_scroll = app.graph_scroll.saturating_add(1);
            } else if app.selected + 1 < app.rows.len() {
                app.selected += 1;
                reload_selected_dist_info(app);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.tab == Tab::Search {
                if app.search_selected > 0 {
                    app.search_selected -= 1;
                }
            } else if app.tab == Tab::Graph {
                app.graph_scroll = app.graph_scroll.saturating_sub(1);
            } else if app.selected > 0 {
                app.selected -= 1;
                reload_selected_dist_info(app);
            }
        }
        KeyCode::PageDown => {
            if app.tab == Tab::Dependencies {
                app.detail_readme_scroll = app.detail_readme_scroll.saturating_add(10);
            } else if app.tab == Tab::Search {
                app.readme_scroll = app.readme_scroll.saturating_add(10);
            } else if app.tab == Tab::Graph {
                app.graph_scroll = app.graph_scroll.saturating_add(10);
            }
        }
        KeyCode::PageUp => {
            if app.tab == Tab::Dependencies {
                app.detail_readme_scroll = app.detail_readme_scroll.saturating_sub(10);
            } else if app.tab == Tab::Search {
                app.readme_scroll = app.readme_scroll.saturating_sub(10);
            } else if app.tab == Tab::Graph {
                app.graph_scroll = app.graph_scroll.saturating_sub(10);
            }
        }
        KeyCode::Home => {
            if app.tab == Tab::Search {
                app.readme_scroll = 0;
            } else if app.tab == Tab::Dependencies {
                app.detail_readme_scroll = 0;
            } else if app.tab == Tab::Graph {
                app.graph_scroll = 0;
            } else {
                app.selected = 0;
            }
        }
        KeyCode::Char('i') if app.can_manage() => app.action = Some(TuiAction::Install),
        KeyCode::Char('u') if app.can_manage() => app.action = Some(TuiAction::Update),
        KeyCode::Char('a') if app.can_manage() => {
            app.tab = Tab::Search;
            app.input.clear();
            app.input_mode = InputMode::AddUrl;
        }
        KeyCode::Char('r') if app.can_manage() => {
            if let Some(row) = app.selected_row()
                && !matches!(row.kind, DependencyKind::Transitive)
            {
                app.input_mode = InputMode::ConfirmRemove {
                    name: row.name.clone(),
                };
            }
        }
        KeyCode::Char('m') if app.can_manage() && !app.readme_url.is_empty() => {
            app.action = Some(TuiAction::AddModule {
                url: app.readme_url.clone(),
            });
        }
        KeyCode::Char('p') if app.can_manage() && !app.readme_url.is_empty() => {
            app.action = Some(TuiAction::AddPlugin {
                url: app.readme_url.clone(),
            });
        }
        KeyCode::Enter => {
            if app.tab == Tab::Search && app.can_manage() {
                app.action = Some(TuiAction::AddPlugin {
                    url: BUILTIN_PLUGINS[app.search_selected].to_string(),
                });
            }
        }
        _ => {}
    }
}

fn handle_url_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Enter => {
            let url = app.input.trim().to_string();
            if url.is_empty() {
                app.status = "Paste a GitHub repository URL first.".to_string();
            } else {
                app.readme_url = url.clone();
                app.status = format!("Fetching README for {url}");
                match fetch_github_readme(&url) {
                    Ok(readme) => {
                        app.readme = readme;
                        app.readme_scroll = 0;
                        app.status =
                            "README loaded. Press m to add as module, or p to add as plugin."
                                .to_string();
                    }
                    Err(err) => {
                        app.readme = format!("{err}");
                        app.status = "Could not load README.".to_string();
                    }
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c) => app.input.push(c),
        _ => {}
    }
}

fn handle_remove_confirm_key(app: &mut App, code: KeyCode, name: String) {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => app.action = Some(TuiAction::Remove { name }),
        _ => app.input_mode = InputMode::Normal,
    }
}

fn fetch_github_readme(url: &str) -> Result<String> {
    let (owner, repo) = parse_github_owner_repo(url).ok_or_else(|| {
        QuiverError::Other("expected a GitHub URL like https://github.com/owner/repo".to_string())
    })?;
    let api_url = format!("https://api.github.com/repos/{owner}/{repo}/readme");
    let output = Command::new("curl")
        .arg("-fsSL")
        .arg("-H")
        .arg("Accept: application/vnd.github.raw")
        .arg("-H")
        .arg("User-Agent: quiver")
        .arg(api_url)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(QuiverError::Other(format!(
            "failed to fetch README for {owner}/{repo}: {}",
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    let trimmed = url
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let rest = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("git@github.com:"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

fn kind_label(kind: &DependencyKind) -> &'static str {
    match kind {
        DependencyKind::Module => "module",
        DependencyKind::Plugin => "plugin",
        DependencyKind::Transitive => "transitive",
    }
}

fn kind_style(kind: &DependencyKind) -> Style {
    match kind {
        DependencyKind::Module => Style::default().fg(Color::Cyan),
        DependencyKind::Plugin => Style::default().fg(Color::Magenta),
        DependencyKind::Transitive => Style::default().fg(Color::Gray),
    }
}

fn short_rev(rev: &str) -> &str {
    rev.get(..12).unwrap_or(rev)
}

fn reload_selected_dist_info(app: &mut App) {
    if let (Some(project_dir), Some(row)) = (&app.project_dir, app.selected_row()) {
        let (readme, license) = load_dist_info_for_row(project_dir, row);
        app.local_readme = readme;
        app.local_license = license;
        app.detail_readme_scroll = 0;
    } else {
        app.local_readme.clear();
        app.local_license.clear();
        app.detail_readme_scroll = 0;
    }
}

fn load_dist_info_for_row(project_dir: &Path, row: &DependencyRow) -> (String, String) {
    let mut readme = String::new();
    let mut license = String::new();

    if let Some(dist_info) = dist_info_path(project_dir, row) {
        readme = read_local_readme(&dist_info);
        license = read_local_license(&dist_info);
    }

    (readme, license)
}

fn dist_info_path(project_dir: &Path, row: &DependencyRow) -> Option<PathBuf> {
    let rev = row.locked_rev.as_ref()?;
    let tag = row
        .locked_tag
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(&rev[..12.min(rev.len())]);

    let mut version = String::with_capacity(tag.len());
    for ch in tag.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            version.push(ch);
        } else {
            version.push('_');
        }
    }

    if version.is_empty() {
        version.push_str("unknown");
    }

    let dir_name = format!("{}-{version}.dist-info", row.name);
    Some(project_dir.join(".nu-env").join("modules").join(dir_name))
}

fn read_local_readme(dist_info_dir: &Path) -> String {
    for entry in std::fs::read_dir(dist_info_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let base = name.split('.').next().unwrap_or(&name);
        if base == "readme" {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                return content;
            }
        }
    }
    String::new()
}

fn read_local_license(dist_info_dir: &Path) -> String {
    let nupackage = dist_info_dir.join("nupackage.toml");
    if let Ok(content) = std::fs::read_to_string(&nupackage) {
        if let Ok(manifest) = crate::manifest::Manifest::from_str(&content) {
            if let Some(license) = manifest.package.license {
                let trimmed = license.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }

    for entry in std::fs::read_dir(dist_info_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let base = name.split('.').next().unwrap_or(&name);
        if matches!(base, "license" | "licenses" | "copying") {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                let upper = content.to_ascii_uppercase();
                if upper.contains("MIT") {
                    return "MIT".to_string();
                }
                if upper.contains("APACHE") {
                    return "Apache-2.0".to_string();
                }
                if upper.contains("GPL") {
                    return "GPL".to_string();
                }
                if upper.contains("BSD") {
                    return "BSD".to_string();
                }
                return "Unknown".to_string();
            }
        }
    }

    String::new()
}

fn next_tab(tab: Tab) -> Tab {
    match tab {
        Tab::Dependencies => Tab::Graph,
        Tab::Graph => Tab::Search,
        Tab::Search => Tab::Dependencies,
    }
}

fn previous_tab(tab: Tab) -> Tab {
    match tab {
        Tab::Dependencies => Tab::Search,
        Tab::Graph => Tab::Dependencies,
        Tab::Search => Tab::Graph,
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_github_url() {
        assert_eq!(
            parse_github_owner_repo("https://github.com/freepicheep/quiver.git/"),
            Some(("freepicheep".to_string(), "quiver".to_string()))
        );
    }

    #[test]
    fn parse_github_url_with_extra_path() {
        assert_eq!(
            parse_github_owner_repo("https://github.com/freepicheep/quiver/tree/main"),
            Some(("freepicheep".to_string(), "quiver".to_string()))
        );
    }

    #[test]
    fn rejects_non_github_url() {
        assert_eq!(parse_github_owner_repo("https://example.com/a/b"), None);
    }

    #[test]
    fn test_dist_info_path() {
        let row = DependencyRow {
            name: "nu-salesforce".to_string(),
            kind: DependencyKind::Module,
            git: "test".to_string(),
            requested: "test".to_string(),
            locked_rev: Some("0123456789abcdef".to_string()),
            locked_tag: Some("v0.3.0".to_string()),
            checksum: None,
        };
        let path = dist_info_path(Path::new("/project"), &row).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/project/.nu-env/modules/nu-salesforce-v0.3.0.dist-info")
        );

        let row_no_tag = DependencyRow {
            name: "nu-salesforce".to_string(),
            kind: DependencyKind::Module,
            git: "test".to_string(),
            requested: "test".to_string(),
            locked_rev: Some("0123456789abcdef".to_string()),
            locked_tag: None,
            checksum: None,
        };
        let path_no_tag = dist_info_path(Path::new("/project"), &row_no_tag).unwrap();
        assert_eq!(
            path_no_tag,
            PathBuf::from("/project/.nu-env/modules/nu-salesforce-0123456789ab.dist-info")
        );
    }

    #[test]
    fn test_read_local_license() {
        let temp_dir = std::env::temp_dir().join("quiver_test_license");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        std::fs::write(temp_dir.join("LICENSE"), "This is an MIT license.\n").unwrap();
        assert_eq!(read_local_license(&temp_dir), "MIT");

        std::fs::write(
            temp_dir.join("nupackage.toml"),
            r#"[package]
name = "test"
version = "0.1.0"
license = "Apache-2.0"
"#,
        )
        .unwrap();
        assert_eq!(read_local_license(&temp_dir), "Apache-2.0");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
