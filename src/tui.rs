use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, mpsc};
use std::thread;
use std::time::Duration;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use leaves::theme::TERMINAL;
use leaves::{
    MarkdownTheme, parse_markdown_with_width, syntax_set_with_bundled_syntaxes,
    theme_set_with_bundled_themes,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap,
    },
};
use syntect::{highlighting::ThemeSet, parsing::SyntaxSet};
use tui_scrollview::{ScrollView, ScrollViewState, ScrollbarVisibility};

use crate::config::{self, GlobalConfig};
use crate::error::{QuiverError, Result};
use crate::lockfile::{LockedPackageKind, Lockfile};
use crate::manifest::{DependencySpec, Manifest, PluginDependencySpec};
use crate::ui::{CapturedLog, LogKind};

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

const LOG_PANEL_HEIGHT: u16 = 8;
const HEADER_TAB_PADDING_LEFT: &str = " ";
const HEADER_TAB_PADDING_RIGHT: &str = " ";
const HEADER_TAB_DIVIDER: &str = "|";

static MARKDOWN_SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(|| {
    syntax_set_with_bundled_syntaxes().unwrap_or_else(|err| {
        eprintln!("warning: failed to load bundled markdown syntaxes: {err}");
        SyntaxSet::load_defaults_newlines()
    })
});
static MARKDOWN_THEME_SET: LazyLock<ThemeSet> = LazyLock::new(|| {
    theme_set_with_bundled_themes().unwrap_or_else(|err| {
        eprintln!("warning: failed to load bundled markdown themes: {err}");
        ThemeSet::load_defaults()
    })
});
static MARKDOWN_THEME: LazyLock<MarkdownTheme> = LazyLock::new(|| TERMINAL);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    Init,
    Install,
    Update,
    Remove {
        name: String,
    },
    AddModule {
        url: String,
        tag: Option<String>,
        rev: Option<String>,
        branch: Option<String>,
    },
    AddPlugin {
        url: String,
        tag: Option<String>,
        rev: Option<String>,
        branch: Option<String>,
        bin: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dependencies,
    Graph,
    Search,
}

const HEADER_TABS: [(Tab, &str); 3] = [
    (Tab::Dependencies, "Dependencies"),
    (Tab::Graph, "Graph"),
    (Tab::Search, "Add"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    ConfirmInit,
    AddUrl,
    BuiltinPlugins,
    ChooseRef {
        target: AddTarget,
    },
    AddRefValue {
        target: AddTarget,
        ref_kind: RefInputKind,
    },
    ConfirmRemove {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AddTarget {
    Module { url: String },
    Plugin { url: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefInputKind {
    Tag,
    Rev,
    Branch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefChoice {
    Auto,
    Tag,
    Branch,
    Rev,
}

const REF_CHOICES: [RefChoice; 4] = [
    RefChoice::Auto,
    RefChoice::Tag,
    RefChoice::Branch,
    RefChoice::Rev,
];

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum LogLine {
    Plain(String),
    Command(String),
    Captured(CapturedLog),
}

enum TuiCommandEvent {
    Log(CapturedLog),
    Finished(std::result::Result<(), String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusRegion {
    Header,
    DependencyList,
    DependencyDetails,
    DependencyReadme,
    Graph,
    SearchUrl,
    SearchReadme,
    BuiltinPlugins,
    CommandLog,
    Footer,
}

struct App {
    cwd: PathBuf,
    project_dir: Option<PathBuf>,
    global: bool,
    package_name: String,
    package_version: String,
    rows: Vec<DependencyRow>,
    tab: Tab,
    selected: usize,
    search_selected: usize,
    ref_choice_selected: usize,
    readme_scroll: ScrollViewState,
    detail_readme_scroll: ScrollViewState,
    graph_scroll: ScrollViewState,
    local_readme: String,
    local_license: String,
    status: String,
    input: String,
    readme_url: String,
    readme: String,
    input_mode: InputMode,
    action: Option<TuiAction>,
    logs: Vec<LogLine>,
    log_scroll: ScrollViewState,
    follow_log: bool,
    log_visible: bool,
    command_rx: Option<mpsc::Receiver<TuiCommandEvent>>,
    command_running: bool,
    pending_reload: bool,
    focus: FocusRegion,
    regions: Vec<(FocusRegion, Rect)>,
    quit: bool,
}

pub fn run(
    cwd: &Path,
    global: bool,
    run_action: impl Fn(TuiAction, Box<dyn FnMut(CapturedLog) + Send>) -> Result<()>
    + Send
    + Sync
    + 'static,
) -> Result<()> {
    if !io::stderr().is_terminal() {
        return Err(QuiverError::Other(
            "qv without a subcommand opens the TUI; run it in an interactive terminal".to_string(),
        ));
    }

    let mut app = App::load(cwd, global)?;
    let run_action = Arc::new(run_action);

    enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let tui_result = (|| -> Result<()> {
        loop {
            drain_command_events(&mut app)?;
            terminal.draw(|frame| render(frame, &mut app))?;

            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        handle_key(&mut app, key.code, key.modifiers);
                    }
                    Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
                    _ => {}
                }
            }
            if let Some(action) = app.action.take() {
                start_tui_action(&mut app, action, Arc::clone(&run_action));
            }
            if app.quit {
                break;
            }
        }

        Ok(())
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
        (Ok(()), Ok(())) => Ok(()),
        (Err(err), Ok(())) => Err(err),
        (Ok(()), Err(err)) | (Err(_), Err(err)) => Err(err.into()),
    }
}

impl App {
    fn load(cwd: &Path, global: bool) -> Result<Self> {
        if global {
            let global_config = GlobalConfig::load_or_default()?;
            let lockfile = read_global_lockfile()?;
            let rows = dependency_rows_from(
                global_config.modules.iter(),
                global_config.plugins.iter(),
                lockfile.as_ref(),
            );
            let status = if rows.is_empty() {
                "Global config is empty. Press a to add a module from GitHub.".to_string()
            } else {
                "Editing global config. a adds, r removes, i installs.".to_string()
            };
            let modules_dir = global_config.modules_dir().ok();
            let (local_readme, local_license) = rows
                .first()
                .and_then(|row| {
                    modules_dir
                        .as_deref()
                        .map(|dir| load_dist_info_at(dir, row))
                })
                .unwrap_or_default();
            return Ok(Self {
                cwd: cwd.to_path_buf(),
                project_dir: None,
                global: true,
                package_name: "global config".to_string(),
                package_version: String::new(),
                rows,
                tab: Tab::Dependencies,
                selected: 0,
                search_selected: 0,
                ref_choice_selected: 0,
                readme_scroll: ScrollViewState::new(),
                detail_readme_scroll: ScrollViewState::new(),
                graph_scroll: ScrollViewState::new(),
                local_readme,
                local_license,
                status,
                input: String::new(),
                readme_url: String::new(),
                readme: "Paste a GitHub repository URL, then press Enter to preview its README."
                    .to_string(),
                input_mode: InputMode::Normal,
                action: None,
                logs: vec![LogLine::Plain("Logs will show here".to_string())],
                log_scroll: ScrollViewState::new(),
                follow_log: true,
                log_visible: false,
                command_rx: None,
                command_running: false,
                pending_reload: false,
                focus: FocusRegion::DependencyList,
                regions: Vec::new(),
                quit: false,
            });
        }

        let Some(project_dir) = Manifest::find_project_dir(cwd) else {
            return Ok(Self {
                cwd: cwd.to_path_buf(),
                project_dir: None,
                global: false,
                package_name: "No quiver project".to_string(),
                package_version: String::new(),
                rows: Vec::new(),
                tab: Tab::Dependencies,
                selected: 0,
                search_selected: 0,
                ref_choice_selected: 0,
                readme_scroll: ScrollViewState::new(),
                detail_readme_scroll: ScrollViewState::new(),
                graph_scroll: ScrollViewState::new(),
                local_readme: String::new(),
                local_license: String::new(),
                status: "No quiver project here. Enter creates one, Esc quits.".to_string(),
                input: String::new(),
                readme_url: String::new(),
                readme: "No nupackage.nuon was found in this directory or its parents.".to_string(),
                input_mode: InputMode::ConfirmInit,
                action: None,
                logs: vec![LogLine::Plain(
                    "Open a quiver project to run commands here.".to_string(),
                )],
                log_scroll: ScrollViewState::new(),
                follow_log: true,
                log_visible: false,
                command_rx: None,
                command_running: false,
                pending_reload: false,
                focus: FocusRegion::DependencyList,
                regions: Vec::new(),
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
            cwd: cwd.to_path_buf(),
            project_dir: Some(project_dir),
            global: false,
            package_name: manifest.package.name,
            package_version: manifest.package.version,
            rows,
            tab: Tab::Dependencies,
            selected: 0,
            search_selected: 0,
            ref_choice_selected: 0,
            readme_scroll: ScrollViewState::new(),
            detail_readme_scroll: ScrollViewState::new(),
            graph_scroll: ScrollViewState::new(),
            local_readme,
            local_license,
            status,
            input: String::new(),
            readme_url: String::new(),
            readme: "Paste a GitHub repository URL, then press Enter to preview its README."
                .to_string(),
            input_mode: InputMode::Normal,
            action: None,
            logs: vec![LogLine::Plain("Logs will show here".to_string())],
            log_scroll: ScrollViewState::new(),
            follow_log: true,
            log_visible: false,
            command_rx: None,
            command_running: false,
            pending_reload: false,
            focus: FocusRegion::DependencyList,
            regions: Vec::new(),
            quit: false,
        })
    }

    fn selected_row(&self) -> Option<&DependencyRow> {
        self.rows.get(self.selected)
    }

    fn can_manage(&self) -> bool {
        self.global || self.project_dir.is_some()
    }

    fn reload(&mut self) -> Result<()> {
        if self.global {
            let global_config = GlobalConfig::load_or_default()?;
            let lockfile = read_global_lockfile()?;
            self.rows = dependency_rows_from(
                global_config.modules.iter(),
                global_config.plugins.iter(),
                lockfile.as_ref(),
            );
            if self.selected >= self.rows.len() {
                self.selected = self.rows.len().saturating_sub(1);
            }
            reload_selected_dist_info(self);
            return Ok(());
        }

        let just_initialized = self.project_dir.is_none();
        if just_initialized {
            self.project_dir = Manifest::find_project_dir(&self.cwd);
        }
        let Some(project_dir) = self.project_dir.clone() else {
            return Ok(());
        };
        let manifest = Manifest::from_dir(&project_dir)?;
        let lockfile = read_lockfile(&project_dir)?;
        self.rows = dependency_rows(&manifest, lockfile.as_ref());
        self.package_name = manifest.package.name;
        self.package_version = manifest.package.version;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
        reload_selected_dist_info(self);
        if just_initialized {
            self.readme = "Paste a GitHub repository URL, then press Enter to preview its README."
                .to_string();
        }
        Ok(())
    }

    fn push_log(&mut self, line: LogLine) {
        self.logs.push(line);
        if self.follow_log {
            self.scroll_log_to_bottom();
        }
    }

    fn scroll_log_to_bottom(&mut self) {
        self.log_scroll.scroll_to_bottom();
        self.follow_log = true;
    }

    fn scroll_log_down(&mut self, amount: u16) {
        for _ in 0..amount {
            self.log_scroll.scroll_down();
        }
        self.follow_log = false;
    }

    fn scroll_log_up(&mut self, amount: u16) {
        for _ in 0..amount {
            self.log_scroll.scroll_up();
        }
        self.follow_log = false;
    }

    fn register_region(&mut self, region: FocusRegion, area: Rect) {
        self.regions.push((region, area));
    }

    fn set_log_visible(&mut self, visible: bool) {
        self.log_visible = visible;
        if !visible && self.focus == FocusRegion::CommandLog {
            self.focus = default_focus_for_tab(self.tab);
        }
    }

    fn toggle_log_visible(&mut self) {
        self.set_log_visible(!self.log_visible);
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

fn read_global_lockfile() -> Result<Option<Lockfile>> {
    let path = config::global_lock_path()?;
    if path.exists() {
        Ok(Some(Lockfile::from_path(&path)?))
    } else {
        Ok(None)
    }
}

fn dependency_rows(manifest: &Manifest, lockfile: Option<&Lockfile>) -> Vec<DependencyRow> {
    dependency_rows_from(
        manifest.dependencies.modules.iter(),
        manifest.dependencies.plugins.iter(),
        lockfile,
    )
}

fn dependency_rows_from<'a, M, P>(
    modules_iter: M,
    plugins_iter: P,
    lockfile: Option<&Lockfile>,
) -> Vec<DependencyRow>
where
    M: IntoIterator<Item = (&'a String, &'a DependencySpec)>,
    P: IntoIterator<Item = (&'a String, &'a PluginDependencySpec)>,
{
    let mut locked_by_name = HashMap::new();
    if let Some(lockfile) = lockfile {
        for package in &lockfile.packages {
            locked_by_name.insert((package.name.clone(), package.kind.clone()), package);
        }
    }

    let mut rows = Vec::new();
    let mut direct_names = HashSet::new();

    let mut modules: Vec<_> = modules_iter.into_iter().collect();
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

    let mut plugins: Vec<_> = plugins_iter.into_iter().collect();
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
    app.regions.clear();
    let area = frame.area();
    let log_height = if app.log_visible { LOG_PANEL_HEIGHT } else { 0 };
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(log_height),
            Constraint::Length(3),
        ])
        .split(area);

    render_header(frame, app, shell[0]);
    app.register_region(FocusRegion::Header, shell[0]);
    match app.tab {
        Tab::Dependencies => render_dependencies(frame, app, shell[1]),
        Tab::Graph => {
            app.register_region(FocusRegion::Graph, shell[1]);
            render_graph(frame, app, shell[1]);
        }
        Tab::Search => render_search(frame, app, shell[1]),
    }
    if app.log_visible {
        render_log(frame, app, shell[2]);
        app.register_region(FocusRegion::CommandLog, shell[2]);
    }
    render_footer(frame, app, shell[3]);
    app.register_region(FocusRegion::Footer, shell[3]);

    match &app.input_mode {
        InputMode::ConfirmInit => render_init_confirm(frame, app),
        InputMode::AddUrl => render_input(frame, app, "Paste GitHub repository URL"),
        InputMode::BuiltinPlugins => render_builtin_plugins_dialog(frame, app),
        InputMode::ChooseRef { target } => render_ref_choice(frame, app, target),
        InputMode::AddRefValue { ref_kind, .. } => {
            render_input(frame, app, ref_input_title(*ref_kind))
        }
        InputMode::ConfirmRemove { name } => render_confirm(
            frame,
            &format!("Remove '{name}'? Enter confirms, Esc cancels"),
        ),
        InputMode::Normal => {}
    }
}

fn drain_command_events(app: &mut App) -> Result<()> {
    let Some(rx) = app.command_rx.take() else {
        return Ok(());
    };
    let mut keep_rx = true;

    while let Ok(event) = rx.try_recv() {
        match event {
            TuiCommandEvent::Log(log) => app.push_log(LogLine::Captured(log)),
            TuiCommandEvent::Finished(result) => {
                app.command_running = false;
                keep_rx = false;
                match result {
                    Ok(()) => {
                        app.push_log(LogLine::Captured(CapturedLog {
                            kind: LogKind::Success,
                            message: "command completed".to_string(),
                        }));
                        app.status = "Command completed.".to_string();
                        app.pending_reload = true;
                    }
                    Err(err) => {
                        app.push_log(LogLine::Captured(CapturedLog {
                            kind: LogKind::Error,
                            message: err,
                        }));
                        app.status = "Command failed.".to_string();
                    }
                }
            }
        }
    }

    if keep_rx {
        app.command_rx = Some(rx);
    }

    if app.pending_reload && !app.command_running {
        app.pending_reload = false;
        if let Err(err) = app.reload() {
            app.push_log(LogLine::Captured(CapturedLog {
                kind: LogKind::Error,
                message: format!("failed to reload project: {err}"),
            }));
            app.status = "Command completed, but project reload failed.".to_string();
        }
    }

    Ok(())
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    if let Some(region) = region_at(app, mouse.column, mouse.row) {
        match (mouse.kind, region) {
            (MouseEventKind::Down(MouseButton::Left), FocusRegion::Header) => {
                focus_tab_at(app, mouse.column);
            }
            (MouseEventKind::Down(MouseButton::Left), FocusRegion::DependencyList) => {
                app.focus = region;
                select_dependency_at(app, mouse.row);
            }
            (MouseEventKind::Down(MouseButton::Left), FocusRegion::BuiltinPlugins) => {
                app.focus = region;
                select_builtin_plugin_at(app, mouse.row);
            }
            (MouseEventKind::Down(MouseButton::Left), _) => {
                app.focus = region;
            }
            (MouseEventKind::ScrollDown, _) => {
                app.focus = region;
                scroll_focused(app, 3);
            }
            (MouseEventKind::ScrollUp, _) => {
                app.focus = region;
                scroll_focused(app, -3);
            }
            _ => {}
        }
    } else {
        match mouse.kind {
            MouseEventKind::ScrollDown => scroll_focused(app, 3),
            MouseEventKind::ScrollUp => scroll_focused(app, -3),
            _ => {}
        }
    }
}

fn start_tui_action(
    app: &mut App,
    action: TuiAction,
    run_action: Arc<
        impl Fn(TuiAction, Box<dyn FnMut(CapturedLog) + Send>) -> Result<()> + Send + Sync + 'static,
    >,
) {
    if app.command_running {
        app.status = "A command is already running.".to_string();
        return;
    }

    let label = action.label();
    app.input_mode = InputMode::Normal;
    app.set_log_visible(true);
    app.push_log(LogLine::Command(format!("qv {label}")));
    app.status = format!("Running {label}...");
    app.command_running = true;

    let (tx, rx) = mpsc::channel();
    app.command_rx = Some(rx);

    thread::spawn(move || {
        let log_tx = tx.clone();
        let emit = move |log| {
            let _ = log_tx.send(TuiCommandEvent::Log(log));
        };
        let result = run_action(action, Box::new(emit)).map_err(|err| err.to_string());
        let _ = tx.send(TuiCommandEvent::Finished(result));
    });
}

impl TuiAction {
    fn label(&self) -> String {
        match self {
            TuiAction::Init => "init".to_string(),
            TuiAction::Install => "install".to_string(),
            TuiAction::Update => "update".to_string(),
            TuiAction::Remove { name } => format!("remove {name}"),
            TuiAction::AddModule {
                url,
                tag,
                rev,
                branch,
            } => format!("add {}{}", url, ref_label(tag, rev, branch)),
            TuiAction::AddPlugin {
                url,
                tag,
                rev,
                branch,
                ..
            } => format!("add-plugin {}{}", url, ref_label(tag, rev, branch)),
        }
    }
}

fn ref_label(tag: &Option<String>, rev: &Option<String>, branch: &Option<String>) -> String {
    if let Some(tag) = tag {
        format!(" --tag {tag}")
    } else if let Some(rev) = rev {
        format!(" --rev {}", short_rev(rev))
    } else if let Some(branch) = branch {
        format!(" --branch {branch}")
    } else {
        String::new()
    }
}

fn focused_block<'a>(app: &App, region: FocusRegion, title: impl Into<Line<'a>>) -> Block<'a> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title);
    if app.focus == region {
        block.border_style(Style::default().fg(focus_color()))
    } else {
        block
    }
}

fn wrapped_line_count(lines: &[Line<'static>], width: u16) -> usize {
    if width == 0 {
        return lines.len();
    }
    let w = width as usize;
    lines
        .iter()
        .map(|line| {
            let chars: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            if chars == 0 { 1 } else { chars.div_ceil(w) }
        })
        .sum()
}

fn render_markdown_scrollview(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    content: &str,
    state: &mut ScrollViewState,
) {
    let text_width = area.width.saturating_sub(1).max(1);
    let lines = markdown_lines(content, text_width);
    let content_height = (wrapped_line_count(&lines, text_width) as u16).max(1);

    let mut scroll_view = ScrollView::new(Size::new(text_width, content_height))
        .horizontal_scrollbar_visibility(ScrollbarVisibility::Never)
        .vertical_scrollbar_visibility(ScrollbarVisibility::Automatic);
    scroll_view.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect::new(0, 0, text_width, content_height),
    );
    frame.render_stateful_widget(scroll_view, area, state);
}

fn markdown_lines(content: &str, width: u16) -> Vec<Line<'static>> {
    let render_width = usize::from(width).max(1);
    let syntect_theme = MARKDOWN_THEME_SET
        .themes
        .get("ansi")
        .or_else(|| MARKDOWN_THEME_SET.themes.values().next())
        .expect("syntect default theme set should not be empty");
    let (lines, _) = parse_markdown_with_width(
        content,
        &MARKDOWN_SYNTAX_SET,
        syntect_theme,
        render_width,
        &MARKDOWN_THEME,
    );
    lines
}

fn focus_color() -> Color {
    Color::Green
}

fn region_at(app: &App, x: u16, y: u16) -> Option<FocusRegion> {
    app.regions
        .iter()
        .rev()
        .find_map(|(region, area)| point_in_rect(*area, x, y).then_some(*region))
}

fn point_in_rect(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn region_rect(app: &App, region: FocusRegion) -> Option<Rect> {
    app.regions
        .iter()
        .find_map(|(candidate, area)| (*candidate == region).then_some(*area))
}

fn scroll_focused(app: &mut App, amount: i16) {
    match app.focus {
        FocusRegion::DependencyReadme => offset_scroll(&mut app.detail_readme_scroll, amount),
        FocusRegion::Graph => offset_scroll(&mut app.graph_scroll, amount),
        FocusRegion::SearchReadme => offset_scroll(&mut app.readme_scroll, amount),
        FocusRegion::CommandLog => {
            if amount >= 0 {
                app.scroll_log_down(amount as u16);
            } else {
                app.scroll_log_up(amount.unsigned_abs());
            }
        }
        _ => {}
    }
}

fn offset_scroll(state: &mut ScrollViewState, amount: i16) {
    if amount >= 0 {
        for _ in 0..amount {
            state.scroll_down();
        }
    } else {
        for _ in 0..amount.unsigned_abs() {
            state.scroll_up();
        }
    }
}

fn focus_tab_at(app: &mut App, x: u16) {
    let Some(area) = region_rect(app, FocusRegion::Header) else {
        return;
    };
    if let Some(tab) = header_tab_at(area, x) {
        set_tab(app, tab);
    }
}

fn header_tab_at(area: Rect, x: u16) -> Option<Tab> {
    let inner = inner_block_area(area);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let mut current_x = inner.x;
    for (index, (tab, title)) in HEADER_TABS.iter().enumerate() {
        let tab_start = current_x;
        current_x = current_x.saturating_add(text_width(HEADER_TAB_PADDING_LEFT));
        current_x = current_x.saturating_add(text_width(title));
        current_x = current_x.saturating_add(text_width(HEADER_TAB_PADDING_RIGHT));

        if x >= tab_start && x < current_x {
            return Some(*tab);
        }

        if index + 1 < HEADER_TABS.len() {
            current_x = current_x.saturating_add(text_width(HEADER_TAB_DIVIDER));
        }
    }

    None
}

fn set_tab(app: &mut App, tab: Tab) {
    app.tab = tab;
    app.focus = default_focus_for_tab(tab);
}

fn default_focus_for_tab(tab: Tab) -> FocusRegion {
    match tab {
        Tab::Dependencies => FocusRegion::DependencyList,
        Tab::Graph => FocusRegion::Graph,
        Tab::Search => FocusRegion::SearchReadme,
    }
}

fn inner_block_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn text_width(text: &str) -> u16 {
    text.chars().count() as u16
}

fn select_dependency_at(app: &mut App, y: u16) {
    if app.rows.is_empty() {
        return;
    }
    let Some(area) = region_rect(app, FocusRegion::DependencyList) else {
        return;
    };
    let index = y.saturating_sub(area.y.saturating_add(1)) as usize;
    if index < app.rows.len() {
        app.selected = index;
        reload_selected_dist_info(app);
    }
}

fn select_builtin_plugin_at(app: &mut App, y: u16) {
    let Some(area) = region_rect(app, FocusRegion::BuiltinPlugins) else {
        return;
    };
    let index = y.saturating_sub(area.y.saturating_add(1)) as usize;
    if index < BUILTIN_PLUGINS.len() {
        app.search_selected = index;
    }
}

fn render_header(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let titles = HEADER_TABS
        .iter()
        .map(|(_, title)| Line::from(Span::styled(*title, Style::default().fg(Color::Cyan))))
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
        .block(focused_block(app, FocusRegion::Header, title))
        .divider(HEADER_TAB_DIVIDER)
        .padding(HEADER_TAB_PADDING_LEFT, HEADER_TAB_PADDING_RIGHT)
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
    app.register_region(FocusRegion::DependencyList, chunks[0]);

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
        .block(focused_block(
            app,
            FocusRegion::DependencyList,
            "Dependencies",
        ))
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
    app.register_region(FocusRegion::DependencyDetails, right_chunks[0]);
    app.register_region(FocusRegion::DependencyReadme, right_chunks[1]);

    let detail = selected_detail(app);
    frame.render_widget(
        Paragraph::new(detail)
            .block(focused_block(
                app,
                FocusRegion::DependencyDetails,
                "Details",
            ))
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

    let block = focused_block(app, FocusRegion::DependencyReadme, "README");
    let inner_area = block.inner(right_chunks[1]);
    frame.render_widget(block, right_chunks[1]);
    render_markdown_scrollview(
        frame,
        inner_area,
        readme_content,
        &mut app.detail_readme_scroll,
    );
}

fn selected_detail(app: &App) -> Vec<Line<'static>> {
    let Some(row) = app.selected_row() else {
        return vec![Line::from(
            "Press a to paste a GitHub repository URL and preview its README.",
        )];
    };

    let mut detail = Vec::new();
    detail.push(detail_line("name", &row.name));
    detail.push(detail_line("kind", kind_label(&row.kind)));
    if !app.local_license.is_empty() {
        detail.push(detail_line("license", &app.local_license));
    }
    detail.push(detail_line("source", &row.git));
    detail.push(detail_line("requested", &row.requested));
    if let Some(rev) = &row.locked_rev {
        detail.push(detail_line("locked rev", rev));
    } else {
        detail.push(detail_line("locked rev", "not installed"));
    }
    if let Some(checksum) = &row.checksum {
        detail.push(detail_line("sha256", checksum));
    }
    detail
}

fn detail_line(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key}:"),
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {value}")),
    ])
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

fn render_graph(frame: &mut ratatui::Frame<'_>, app: &mut App, area: Rect) {
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

    let block = focused_block(app, FocusRegion::Graph, "Dependency Graph");
    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<&str> = text.lines().collect();
    let content_width = lines
        .iter()
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .max(inner_area.width);
    let content_height = lines.len() as u16;

    let mut scroll_view = ScrollView::new(Size::new(content_width, content_height))
        .horizontal_scrollbar_visibility(ScrollbarVisibility::Never)
        .vertical_scrollbar_visibility(ScrollbarVisibility::Automatic);
    scroll_view.render_widget(
        Paragraph::new(text),
        Rect::new(0, 0, content_width, content_height),
    );
    frame.render_stateful_widget(scroll_view, inner_area, &mut app.graph_scroll);
}

fn render_search(frame: &mut ratatui::Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);
    app.register_region(FocusRegion::SearchUrl, chunks[0]);
    app.register_region(FocusRegion::SearchReadme, chunks[1]);

    let url = if app.readme_url.is_empty() {
        "No repository loaded".to_string()
    } else {
        app.readme_url.clone()
    };
    frame.render_widget(
        Paragraph::new(url).block(focused_block(
            app,
            FocusRegion::SearchUrl,
            "GitHub Repository",
        )),
        chunks[0],
    );

    let block = focused_block(app, FocusRegion::SearchReadme, "README");
    let inner_area = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);
    render_markdown_scrollview(
        frame,
        inner_area,
        app.readme.as_str(),
        &mut app.readme_scroll,
    );
}

fn render_footer(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let log_hint = if app.log_visible {
        "/ hide log"
    } else {
        "/ show log"
    };
    let keys = match app.tab {
        Tab::Dependencies => {
            format!(
                "q quit | tab switch | j/k select | PgUp/PgDn scroll | {log_hint} | i install | u update | r remove | a add"
            )
        }
        Tab::Graph => format!("q quit | tab switch | d dependencies | a add URL | {log_hint}"),
        Tab::Search => {
            format!(
                "q quit | tab switch | a paste URL | b built-ins | m add repo module | p add repo plugin | {log_hint}"
            )
        }
    };
    let text = format!("{keys}\n{}", app.status);
    frame.render_widget(
        Paragraph::new(text)
            .block(focused_block(app, FocusRegion::Footer, ""))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_log(frame: &mut ratatui::Frame<'_>, app: &mut App, area: Rect) {
    let lines: Vec<Line<'static>> = if app.logs.is_empty() {
        vec![Line::from("No commands run yet.")]
    } else {
        app.logs.iter().map(log_line).collect()
    };
    let title = if app.command_running {
        "Command Log - running"
    } else {
        "Command Log"
    };
    let block = focused_block(app, FocusRegion::CommandLog, title);
    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    let text_width = inner_area.width.saturating_sub(1).max(1);
    let content_height = wrapped_line_count(&lines, text_width).max(1) as u16;
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });

    let mut scroll_view = ScrollView::new(Size::new(text_width, content_height))
        .horizontal_scrollbar_visibility(ScrollbarVisibility::Never)
        .vertical_scrollbar_visibility(ScrollbarVisibility::Automatic);
    scroll_view.render_widget(paragraph, Rect::new(0, 0, text_width, content_height));

    if app.follow_log {
        app.log_scroll.scroll_to_bottom();
    }
    frame.render_stateful_widget(scroll_view, inner_area, &mut app.log_scroll);
}

fn log_line(line: &LogLine) -> Line<'static> {
    match line {
        LogLine::Plain(message) => Line::from(message.clone()),
        LogLine::Command(command) => Line::from(vec![
            Span::styled("$", Style::default().fg(Color::Gray)),
            Span::raw(" "),
            Span::styled(command.clone(), Style::default().fg(Color::Cyan)),
        ]),
        LogLine::Captured(log) => {
            let (label, style) = match log.kind {
                LogKind::Info => (
                    "info",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                LogKind::Success => (
                    "done",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                LogKind::Warn => (
                    "warn",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                LogKind::Error => (
                    "error",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
            };
            Line::from(vec![
                Span::styled(label, style),
                Span::raw(" "),
                Span::raw(log.message.clone()),
            ])
        }
    }
}

fn render_input(frame: &mut ratatui::Frame<'_>, app: &App, title: &str) {
    let area = centered_rect_max(68, 3, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(app.input.as_str()).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(focus_color())),
        ),
        area,
    );
}

fn render_confirm(frame: &mut ratatui::Frame<'_>, message: &str) {
    let area = centered_rect_max(56, 3, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(message).alignment(Alignment::Center).block(
            Block::default()
                .title("Confirm")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(focus_color())),
        ),
        area,
    );
}

fn render_init_confirm(frame: &mut ratatui::Frame<'_>, app: &App) {
    let dir = app.cwd.display().to_string();
    let lines = vec![
        Line::from("No quiver project found here."),
        Line::from(Span::styled(
            format!("Create one in {dir}?"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Enter — create    Esc — cancel"),
    ];
    let area = centered_rect_max(72, 6, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title("New quiver project")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(focus_color())),
            ),
        area,
    );
}

fn render_builtin_plugins_dialog(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = centered_rect_max(44, 12, frame.area());
    frame.render_widget(Clear, area);
    let items: Vec<ListItem> = BUILTIN_PLUGINS
        .iter()
        .map(|p| ListItem::new(Line::from(p.to_string())))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .title("Built-in Plugins - Enter adds, Esc cancels")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(focus_color())),
        )
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let mut list_state = ListState::default();
    list_state.select(Some(app.search_selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_ref_choice(frame: &mut ratatui::Frame<'_>, app: &App, target: &AddTarget) {
    let title = match target {
        AddTarget::Module { .. } => "Add Module Ref",
        AddTarget::Plugin { .. } => "Add Plugin Ref",
    };
    let area = centered_rect_max(62, 8, frame.area());
    frame.render_widget(Clear, area);
    let items: Vec<ListItem> = REF_CHOICES
        .iter()
        .map(|choice| ListItem::new(Line::from(ref_choice_label(*choice))))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(focus_color())),
        )
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let mut list_state = ListState::default();
    list_state.select(Some(app.ref_choice_selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn ref_input_title(ref_kind: RefInputKind) -> &'static str {
    match ref_kind {
        RefInputKind::Tag => "Version tag",
        RefInputKind::Rev => "Revision",
        RefInputKind::Branch => "Branch",
    }
}

fn ref_choice_label(choice: RefChoice) -> &'static str {
    match choice {
        RefChoice::Auto => "auto-detect latest tag/default branch (a)",
        RefChoice::Tag => "version tag (v)",
        RefChoice::Branch => "branch (b)",
        RefChoice::Rev => "revision (r)",
    }
}

fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    match app.input_mode.clone() {
        InputMode::Normal => handle_normal_key(app, code, modifiers),
        InputMode::ConfirmInit => handle_init_confirm_key(app, code),
        InputMode::AddUrl => handle_url_key(app, code),
        InputMode::BuiltinPlugins => handle_builtin_plugins_key(app, code),
        InputMode::ChooseRef { target } => handle_ref_choice_key(app, code, target),
        InputMode::AddRefValue { target, ref_kind } => {
            handle_ref_value_key(app, code, target, ref_kind)
        }
        InputMode::ConfirmRemove { name } => handle_remove_confirm_key(app, code, name),
    }
}

fn handle_init_confirm_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Enter => {
            app.action = Some(TuiAction::Init);
        }
        KeyCode::Esc => app.quit = true,
        _ => {}
    }
}

fn handle_normal_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Tab | KeyCode::Right => set_tab(app, next_tab(app.tab)),
        KeyCode::BackTab | KeyCode::Left => set_tab(app, previous_tab(app.tab)),
        KeyCode::Char('d') => set_tab(app, Tab::Dependencies),
        KeyCode::Char('g') => set_tab(app, Tab::Graph),
        KeyCode::Char('s') => set_tab(app, Tab::Search),
        KeyCode::Down | KeyCode::Char('j') => {
            if app.focus == FocusRegion::BuiltinPlugins {
                if app.search_selected + 1 < BUILTIN_PLUGINS.len() {
                    app.search_selected += 1;
                }
            } else if app.focus == FocusRegion::Graph {
                app.graph_scroll.scroll_down();
            } else if app.focus == FocusRegion::DependencyList && app.selected + 1 < app.rows.len()
            {
                app.selected += 1;
                reload_selected_dist_info(app);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.focus == FocusRegion::BuiltinPlugins {
                if app.search_selected > 0 {
                    app.search_selected -= 1;
                }
            } else if app.focus == FocusRegion::Graph {
                app.graph_scroll.scroll_up();
            } else if app.focus == FocusRegion::DependencyList && app.selected > 0 {
                app.selected -= 1;
                reload_selected_dist_info(app);
            }
        }
        KeyCode::PageDown => scroll_focused(app, 10),
        KeyCode::PageUp => scroll_focused(app, -10),
        KeyCode::Char('/') => app.toggle_log_visible(),
        KeyCode::Char(']') => {
            app.scroll_log_down(3);
        }
        KeyCode::Char('[') => {
            app.scroll_log_up(3);
        }
        KeyCode::End => {
            app.scroll_log_to_bottom();
        }
        KeyCode::Home => {
            if app.focus == FocusRegion::SearchReadme {
                app.readme_scroll.scroll_to_top();
            } else if app.focus == FocusRegion::DependencyReadme {
                app.detail_readme_scroll.scroll_to_top();
            } else if app.focus == FocusRegion::Graph {
                app.graph_scroll.scroll_to_top();
            } else if app.focus == FocusRegion::DependencyList {
                app.selected = 0;
            }
        }
        KeyCode::Char('i') if app.can_manage() => app.action = Some(TuiAction::Install),
        KeyCode::Char('u') if app.can_manage() => app.action = Some(TuiAction::Update),
        KeyCode::Char('a') if app.can_manage() => {
            set_tab(app, Tab::Search);
            app.input.clear();
            app.input_mode = InputMode::AddUrl;
        }
        KeyCode::Char('b') if app.can_manage() && app.tab == Tab::Search => {
            app.input_mode = InputMode::BuiltinPlugins;
            app.status = "Choose a built-in plugin.".to_string();
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
            app.ref_choice_selected = 0;
            app.input_mode = InputMode::ChooseRef {
                target: AddTarget::Module {
                    url: app.readme_url.clone(),
                },
            };
            app.status = "Choose how to pin the module.".to_string();
        }
        KeyCode::Char('p') if app.can_manage() && !app.readme_url.is_empty() => {
            app.ref_choice_selected = 0;
            app.input_mode = InputMode::ChooseRef {
                target: AddTarget::Plugin {
                    url: app.readme_url.clone(),
                },
            };
            app.status = "Choose how to pin the plugin.".to_string();
        }
        _ => {}
    }
}

fn handle_builtin_plugins_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Down | KeyCode::Char('j') => {
            if app.search_selected + 1 < BUILTIN_PLUGINS.len() {
                app.search_selected += 1;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.search_selected > 0 {
                app.search_selected -= 1;
            }
        }
        KeyCode::Enter => {
            app.action = Some(TuiAction::AddPlugin {
                url: BUILTIN_PLUGINS[app.search_selected].to_string(),
                tag: None,
                rev: None,
                branch: None,
                bin: None,
            });
            app.input_mode = InputMode::Normal;
        }
        _ => {}
    }
}

fn handle_ref_choice_key(app: &mut App, code: KeyCode, target: AddTarget) {
    match code {
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Down | KeyCode::Char('j') => {
            if app.ref_choice_selected + 1 < REF_CHOICES.len() {
                app.ref_choice_selected += 1;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.ref_choice_selected > 0 {
                app.ref_choice_selected -= 1;
            }
        }
        KeyCode::Enter => submit_ref_choice(app, target, REF_CHOICES[app.ref_choice_selected]),
        KeyCode::Char('a') => submit_ref_choice(app, target, RefChoice::Auto),
        KeyCode::Char('v') | KeyCode::Char('t') => submit_ref_choice(app, target, RefChoice::Tag),
        KeyCode::Char('r') => submit_ref_choice(app, target, RefChoice::Rev),
        KeyCode::Char('b') => submit_ref_choice(app, target, RefChoice::Branch),
        _ => {}
    }
}

fn submit_ref_choice(app: &mut App, target: AddTarget, choice: RefChoice) {
    match choice {
        RefChoice::Auto => {
            app.action = Some(action_for_target(target, None, None, None));
            app.input_mode = InputMode::Normal;
        }
        RefChoice::Tag => start_ref_value_input(app, target, RefInputKind::Tag),
        RefChoice::Rev => start_ref_value_input(app, target, RefInputKind::Rev),
        RefChoice::Branch => start_ref_value_input(app, target, RefInputKind::Branch),
    }
}

fn start_ref_value_input(app: &mut App, target: AddTarget, ref_kind: RefInputKind) {
    app.input.clear();
    app.input_mode = InputMode::AddRefValue { target, ref_kind };
}

fn handle_ref_value_key(app: &mut App, code: KeyCode, target: AddTarget, ref_kind: RefInputKind) {
    match code {
        KeyCode::Enter => {
            let value = app.input.trim().to_string();
            if value.is_empty() {
                app.status = format!(
                    "Enter a {} first.",
                    ref_input_title(ref_kind).to_lowercase()
                );
                return;
            }
            let (tag, rev, branch) = match ref_kind {
                RefInputKind::Tag => (Some(value), None, None),
                RefInputKind::Rev => (None, Some(value), None),
                RefInputKind::Branch => (None, None, Some(value)),
            };
            app.action = Some(action_for_target(target, tag, rev, branch));
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

fn action_for_target(
    target: AddTarget,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
) -> TuiAction {
    match target {
        AddTarget::Module { url } => TuiAction::AddModule {
            url,
            tag,
            rev,
            branch,
        },
        AddTarget::Plugin { url } => TuiAction::AddPlugin {
            url,
            tag,
            rev,
            branch,
            bin: None,
        },
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
                        app.readme_scroll.scroll_to_top();
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
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.action = Some(TuiAction::Remove { name });
        }
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            app.input_mode = InputMode::Normal;
        }
        _ => {}
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
    let base_dir = if app.global {
        GlobalConfig::load_or_default()
            .ok()
            .and_then(|cfg| cfg.modules_dir().ok())
    } else {
        app.project_dir
            .as_deref()
            .map(|dir| dir.join(".nu-env").join("modules"))
    };
    if let (Some(base_dir), Some(row)) = (base_dir, app.selected_row()) {
        let (readme, license) = load_dist_info_at(&base_dir, row);
        app.local_readme = readme;
        app.local_license = license;
        app.detail_readme_scroll.scroll_to_top();
    } else {
        app.local_readme.clear();
        app.local_license.clear();
        app.detail_readme_scroll.scroll_to_top();
    }
}

fn load_dist_info_for_row(project_dir: &Path, row: &DependencyRow) -> (String, String) {
    load_dist_info_at(&project_dir.join(".nu-env").join("modules"), row)
}

fn load_dist_info_at(modules_dir: &Path, row: &DependencyRow) -> (String, String) {
    let mut readme = String::new();
    let mut license = String::new();

    if let Some(dist_info) = dist_info_path_in(modules_dir, row) {
        readme = read_local_readme(&dist_info);
        license = read_local_license(&dist_info);
    }

    (readme, license)
}

#[cfg(test)]
fn dist_info_path(project_dir: &Path, row: &DependencyRow) -> Option<PathBuf> {
    dist_info_path_in(&project_dir.join(".nu-env").join("modules"), row)
}

fn dist_info_path_in(modules_dir: &Path, row: &DependencyRow) -> Option<PathBuf> {
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
    Some(modules_dir.join(dir_name))
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
    let manifest_path = dist_info_dir.join("nupackage.nuon");
    if let Ok(content) = std::fs::read_to_string(&manifest_path) {
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

fn centered_rect_max(max_width: u16, max_height: u16, area: Rect) -> Rect {
    let width = max_width.min(area.width.saturating_sub(4).max(1));
    let height = max_height.min(area.height.saturating_sub(2).max(1));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App {
            cwd: PathBuf::from("/project"),
            project_dir: Some(PathBuf::from("/project")),
            global: false,
            package_name: "test".to_string(),
            package_version: "0.1.0".to_string(),
            rows: Vec::new(),
            tab: Tab::Dependencies,
            selected: 0,
            search_selected: 0,
            ref_choice_selected: 0,
            readme_scroll: ScrollViewState::new(),
            detail_readme_scroll: ScrollViewState::new(),
            graph_scroll: ScrollViewState::new(),
            local_readme: String::new(),
            local_license: String::new(),
            status: String::new(),
            input: String::new(),
            readme_url: String::new(),
            readme: String::new(),
            input_mode: InputMode::Normal,
            action: None,
            logs: Vec::new(),
            log_scroll: ScrollViewState::new(),
            follow_log: true,
            log_visible: false,
            command_rx: None,
            command_running: false,
            pending_reload: false,
            focus: FocusRegion::DependencyList,
            regions: Vec::new(),
            quit: false,
        }
    }

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
            temp_dir.join("nupackage.nuon"),
            r#"{ package: { name: "test", version: "0.1.0", license: "Apache-2.0" } }"#,
        )
        .unwrap();
        assert_eq!(read_local_license(&temp_dir), "Apache-2.0");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn header_tab_at_matches_rendered_titles() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 3,
        };

        assert_eq!(header_tab_at(area, 1), Some(Tab::Dependencies));
        assert_eq!(header_tab_at(area, 17), Some(Tab::Graph));
        assert_eq!(header_tab_at(area, 25), Some(Tab::Search));
        assert_eq!(header_tab_at(area, 15), None);
    }

    #[test]
    fn tab_mouse_release_does_not_focus_header() {
        let mut app = test_app();
        app.regions.push((
            FocusRegion::Header,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 3,
            },
        ));

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 17,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert_eq!(app.focus, FocusRegion::DependencyList);
    }

    #[test]
    fn tab_mouse_down_switches_tab_without_focusing_header() {
        let mut app = test_app();
        app.regions.push((
            FocusRegion::Header,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 3,
            },
        ));

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 17,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert_eq!(app.tab, Tab::Graph);
        assert_eq!(app.focus, FocusRegion::Graph);
    }

    #[test]
    fn start_tui_action_reveals_log() {
        let mut app = test_app();

        start_tui_action(&mut app, TuiAction::Install, Arc::new(|_, _| Ok(())));

        assert!(app.log_visible);
    }

    #[test]
    fn remove_confirm_enter_submits_action() {
        let mut app = test_app();

        handle_remove_confirm_key(&mut app, KeyCode::Enter, "demo".to_string());

        assert_eq!(
            app.action,
            Some(TuiAction::Remove {
                name: "demo".to_string()
            })
        );
    }

    #[test]
    fn hiding_log_moves_focus_back_to_current_tab() {
        let mut app = test_app();
        app.focus = FocusRegion::CommandLog;
        app.log_visible = true;

        app.set_log_visible(false);

        assert_eq!(app.focus, FocusRegion::DependencyList);
    }

    #[test]
    fn add_tab_b_opens_builtin_plugin_dialog() {
        let mut app = test_app();
        app.tab = Tab::Search;

        handle_normal_key(&mut app, KeyCode::Char('b'), KeyModifiers::NONE);

        assert_eq!(app.input_mode, InputMode::BuiltinPlugins);
    }

    #[test]
    fn builtin_plugin_dialog_enter_adds_selected_core_plugin() {
        let mut app = test_app();
        app.search_selected = 2;
        app.input_mode = InputMode::BuiltinPlugins;

        handle_builtin_plugins_key(&mut app, KeyCode::Enter);

        assert_eq!(
            app.action,
            Some(TuiAction::AddPlugin {
                url: BUILTIN_PLUGINS[2].to_string(),
                tag: None,
                rev: None,
                branch: None,
                bin: None,
            })
        );
    }

    #[test]
    fn repo_add_can_choose_branch_before_submitting() {
        let mut app = test_app();
        app.readme_url = "https://github.com/nushell/nu_scripts".to_string();

        handle_normal_key(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(
            app.input_mode,
            InputMode::ChooseRef {
                target: AddTarget::Module {
                    url: "https://github.com/nushell/nu_scripts".to_string()
                }
            }
        );

        handle_ref_choice_key(
            &mut app,
            KeyCode::Char('b'),
            AddTarget::Module {
                url: "https://github.com/nushell/nu_scripts".to_string(),
            },
        );
        app.input = "main".to_string();
        handle_ref_value_key(
            &mut app,
            KeyCode::Enter,
            AddTarget::Module {
                url: "https://github.com/nushell/nu_scripts".to_string(),
            },
            RefInputKind::Branch,
        );

        assert_eq!(
            app.action,
            Some(TuiAction::AddModule {
                url: "https://github.com/nushell/nu_scripts".to_string(),
                tag: None,
                rev: None,
                branch: Some("main".to_string()),
            })
        );
    }

    #[test]
    fn load_without_project_opens_init_confirm_dialog() {
        let temp = std::env::temp_dir().join("quiver_test_no_project_tui");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let app = App::load(&temp, false).unwrap();

        assert!(app.project_dir.is_none());
        assert_eq!(app.input_mode, InputMode::ConfirmInit);

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn init_confirm_enter_submits_init_action() {
        let mut app = test_app();
        app.project_dir = None;
        app.input_mode = InputMode::ConfirmInit;

        handle_init_confirm_key(&mut app, KeyCode::Enter);

        assert_eq!(app.action, Some(TuiAction::Init));
        assert!(!app.quit);
    }

    #[test]
    fn init_confirm_esc_quits_tui() {
        let mut app = test_app();
        app.project_dir = None;
        app.input_mode = InputMode::ConfirmInit;

        handle_init_confirm_key(&mut app, KeyCode::Esc);

        assert!(app.quit);
        assert!(app.action.is_none());
    }

    #[test]
    fn ref_choice_dialog_supports_jk_navigation_and_enter() {
        let mut app = test_app();
        let target = AddTarget::Plugin {
            url: "https://github.com/nushell/nu_plugin_inc".to_string(),
        };

        handle_ref_choice_key(&mut app, KeyCode::Char('j'), target.clone());
        handle_ref_choice_key(&mut app, KeyCode::Char('j'), target.clone());

        assert_eq!(app.ref_choice_selected, 2);

        handle_ref_choice_key(&mut app, KeyCode::Char('k'), target.clone());

        assert_eq!(app.ref_choice_selected, 1);

        handle_ref_choice_key(&mut app, KeyCode::Enter, target);

        assert_eq!(
            app.input_mode,
            InputMode::AddRefValue {
                target: AddTarget::Plugin {
                    url: "https://github.com/nushell/nu_plugin_inc".to_string(),
                },
                ref_kind: RefInputKind::Tag,
            }
        );
    }
}
