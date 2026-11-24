//! The full-screen `status --tui`: what `status` prints, drawn on a terminal and changed with keys.
//!
//! The screen draws the [`View`] or [`ClusterView`] that `status` builds for its text and JSON
//! outputs, so a section an extension adds is a panel here without a change to this module. A panel
//! is the table of the items of its section and, for the selected item that has a rate, a line
//! chart of the rates of the last minute, kept here for as long as the view runs.

use crate::status::{number, reading_text, ClusterView, Data, Reading, ShownItem, ShownSection, View};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Row, Table, Wrap};
use ratatui::Frame;
use std::collections::{BTreeMap, VecDeque};

/// How many samples, one a second, a chart keeps.
const WINDOW: usize = 60;

/// What a key asks of the loop that owns the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Stay,
    Quit,
}

/// Whose rate a history keeps: the member (or `node`, or `all`), the section and the item.
type Key = (String, String, String);

/// The rates of the last [`WINDOW`] samples of every item that has a rate.
#[derive(Debug, Clone, PartialEq, Default)]
struct History(BTreeMap<Key, VecDeque<f64>>);

/// The rates a view has: a sample whose counters were reset by a restart counts as 0.
fn rates(view: &View) -> impl Iterator<Item = (&str, &str, f64)> {
    view.sections.iter().flat_map(move |section| {
        section.items.iter().filter_map(move |item| match &item.reading {
            Reading::Rate { rate, .. } => Some((section.key.as_str(), item.key.as_str(), if view.counter_reset { 0.0 } else { *rate })),
            Reading::Value(_) => None,
        })
    })
}

impl History {
    /// Adds the rates of a sample, and forgets the items it no longer has.
    fn add(&mut self, data: &Data) {
        let mut now: BTreeMap<Key, f64> = BTreeMap::new();
        let mut note = |who: &str, view: &View| {
            for (section, item, rate) in rates(view) {
                *now.entry((who.to_string(), section.to_string(), item.to_string())).or_insert(0.0) += rate;
            }
        };
        match data {
            Data::Node(Ok(view)) => note("node", view),
            Data::Node(Err(_)) => {}
            Data::Cluster(cluster) => {
                for member in &cluster.members {
                    if let Ok(view) = &member.shown {
                        note(&member.id.to_string(), view);
                        note("all", view);
                    }
                }
            }
        }
        self.0.retain(|key, _| now.contains_key(key));
        for (key, rate) in now {
            let kept = self.0.entry(key).or_default();
            if kept.len() >= WINDOW {
                kept.pop_front();
            }
            kept.push_back(rate);
        }
    }

    fn get(&self, who: &str, section: &str, item: &str) -> Option<&VecDeque<f64>> {
        self.0.get(&(who.to_string(), section.to_string(), item.to_string()))
    }
}

/// The view of all the members that answered: each rate and total added up.
fn all_view(cluster: &ClusterView) -> Result<View, String> {
    let views: Vec<&View> = cluster.members.iter().filter_map(|member| member.shown.as_ref().ok()).collect();
    let Some(first) = views.first() else {
        return Err("no member answered".to_string());
    };
    let mut all = (*first).clone();
    all.counter_reset = false;
    for section in &mut all.sections {
        for item in &mut section.items {
            if let Reading::Rate { .. } = item.reading {
                let (mut rate, mut total) = (0.0, 0.0);
                for view in &views {
                    let found = view.sections.iter().filter(|s| s.key == section.key).flat_map(|s| s.items.iter()).find(|i| i.key == item.key);
                    if let Some(ShownItem { reading: Reading::Rate { rate: r, total: t }, .. }) = found {
                        rate += r;
                        total += t;
                    }
                }
                item.reading = Reading::Rate { rate, total };
            }
        }
    }
    Ok(all)
}

/// What is on the screen: the newest data, its sum over the members, and the rates so far.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    data: Data,
    all: Option<Result<View, String>>,
    history: History,
}

impl Shown {
    fn new(data: Data) -> Self {
        let mut shown = Shown { data: Data::Node(Err(String::new())), all: None, history: History::default() };
        shown.update(data);
        shown
    }

    fn update(&mut self, data: Data) {
        self.history.add(&data);
        self.all = match &data {
            Data::Cluster(cluster) => Some(all_view(cluster)),
            Data::Node(_) => None,
        };
        self.data = data;
    }
}

/// The data and what is selected in it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Screen {
    /// What has come in, however long the view is paused.
    live: Shown,
    /// What was on the screen when it was paused.
    paused: Option<Shown>,
    panel: usize,
    /// In a cluster 0 is all the members and `n` is the member `n - 1`.
    choice: usize,
    /// The key of the selected item; the first item when the section has no such item.
    item: Option<String>,
}

impl Screen {
    pub(crate) fn new(data: Data) -> Self {
        Screen { live: Shown::new(data), paused: None, panel: 0, choice: 0, item: None }
    }

    /// What the screen draws: the data as of now, or as of the pause.
    fn shown(&self) -> &Shown {
        self.paused.as_ref().unwrap_or(&self.live)
    }

    /// Shows newer data, unless paused, and keeps the selection where it still exists; the rates
    /// are kept either way.
    pub(crate) fn set_data(&mut self, data: Data) {
        self.live.update(data);
        if self.paused.is_none() {
            self.keep_selection();
        }
    }

    fn keep_selection(&mut self) {
        if let Data::Cluster(cluster) = &self.shown().data {
            self.choice = self.choice.min(cluster.members.len());
        }
        self.panel = self.panel.min(self.panel_count().saturating_sub(1));
    }

    /// The view of the node, of all the members or of the selected one, when it has one.
    fn selected(&self) -> Option<&Result<View, String>> {
        let shown = self.shown();
        match &shown.data {
            Data::Node(view) => Some(view),
            Data::Cluster(cluster) => match self.choice {
                0 => shown.all.as_ref(),
                n => cluster.members.get(n - 1).map(|member| &member.shown),
            },
        }
    }

    /// Whose history the selection has.
    fn who(&self) -> String {
        match &self.shown().data {
            Data::Node(_) => "node".to_string(),
            Data::Cluster(cluster) => match self.choice {
                0 => "all".to_string(),
                n => cluster.members.get(n - 1).map_or_else(String::new, |member| member.id.to_string()),
            },
        }
    }

    fn section(&self) -> Option<&ShownSection> {
        match self.selected() {
            Some(Ok(view)) => view.sections.get(self.panel),
            _ => None,
        }
    }

    fn panel_count(&self) -> usize {
        match self.selected() {
            Some(Ok(view)) => view.sections.len(),
            _ => 0,
        }
    }

    /// The position of the selected item in the section.
    fn item_index(&self) -> usize {
        let Some(section) = self.section() else { return 0 };
        self.item.as_ref().and_then(|key| section.items.iter().position(|item| &item.key == key)).unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn panel(&self) -> usize {
        self.panel
    }

    /// 0 for all the members, else the position of the member plus one.
    #[cfg(test)]
    pub(crate) fn choice(&self) -> usize {
        self.choice
    }

    #[cfg(test)]
    pub(crate) fn item_key(&self) -> Option<&str> {
        self.section().and_then(|section| section.items.get(self.item_index())).map(|item| item.key.as_str())
    }

    /// Moves the selected item by `by`, and stays on the first and the last.
    fn move_item(&mut self, by: isize) {
        let Some(section) = self.section() else { return };
        let last = section.items.len().saturating_sub(1);
        let index = self.item_index().saturating_add_signed(by).min(last);
        self.item = section.items.get(index).map(|item| item.key.clone());
    }

    /// Applies a key.
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Step {
        if key.kind == KeyEventKind::Release {
            return Step::Stay;
        }
        let count = self.panel_count();
        let members = match &self.shown().data {
            Data::Cluster(cluster) => Some(cluster.members.len()),
            Data::Node(_) => None,
        };
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Step::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Step::Quit,
            KeyCode::Right | KeyCode::Tab if count > 0 => self.panel = (self.panel + 1) % count,
            KeyCode::Left | KeyCode::BackTab if count > 0 => self.panel = (self.panel + count - 1) % count,
            KeyCode::Down => self.move_item(1),
            KeyCode::Up => self.move_item(-1),
            KeyCode::Char(']') => {
                if let Some(members) = members {
                    self.choice = (self.choice + 1).min(members);
                    self.panel = self.panel.min(self.panel_count().saturating_sub(1));
                }
            }
            KeyCode::Char('[') => {
                if members.is_some() {
                    self.choice = self.choice.saturating_sub(1);
                    self.panel = self.panel.min(self.panel_count().saturating_sub(1));
                }
            }
            KeyCode::Char('p') => {
                if self.paused.take().is_some() {
                    self.keep_selection();
                } else {
                    self.paused = Some(self.live.clone());
                }
            }
            _ => {}
        }
        Step::Stay
    }
}

/// Gives the terminal back as it was when dropped, whatever ended the view.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Shows the screen until the user leaves it with a key; fed with newer data from `rx`. Returns the
/// exit status of the command.
pub(crate) async fn run(mut rx: tokio::sync::mpsc::UnboundedReceiver<Data>) -> i32 {
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(e) => {
            eprintln!("status --tui could not use the terminal: {e}");
            return 2;
        }
    };
    let restore = Restore;
    let (keys_tx, mut keys) = tokio::sync::mpsc::unbounded_channel();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match ratatui::crossterm::event::poll(std::time::Duration::from_millis(100)) {
                    Ok(true) => match ratatui::crossterm::event::read() {
                        Ok(ratatui::crossterm::event::Event::Key(key)) => {
                            if keys_tx.send(key).is_err() {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        })
    };
    let mut screen = Screen::new(Data::Node(Err("waiting for the first two samples".to_string())));
    let status = loop {
        if let Err(e) = terminal.draw(|frame| draw(frame, &screen)) {
            drop(restore);
            eprintln!("status --tui could not draw: {e}");
            break 2;
        }
        tokio::select! {
            data = rx.recv() => match data {
                Some(data) => screen.set_data(data),
                None => break 0,
            },
            key = keys.recv() => match key {
                Some(key) => {
                    if screen.on_key(key) == Step::Quit {
                        break 0;
                    }
                }
                None => break 2,
            },
        }
    };
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = reader.join();
    status
}

const HELP: &str = " Up/Down item  Left/Right panel  [ ] member  p pause  q quit";

/// Draws the screen.
pub(crate) fn draw(frame: &mut Frame, screen: &Screen) {
    let members = match &screen.shown().data {
        Data::Cluster(cluster) => u16::try_from(cluster.members.len() + 3).unwrap_or(u16::MAX),
        Data::Node(_) => 0,
    };
    let [head, table, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Length(members), Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let paused = if screen.paused.is_some() { " (paused)" } else { "" };
    let title = match &screen.shown().data {
        Data::Node(Ok(view)) => format!("rradius status of {} over {:.1} s{paused}", view.address, view.interval),
        Data::Node(Err(_)) => format!("rradius status{paused}"),
        Data::Cluster(cluster) => format!("rradius cluster status over {:.1} s{paused}", cluster.interval),
    };
    frame.render_widget(Paragraph::new(Span::styled(title, bold)), head);
    if let Data::Cluster(cluster) = &screen.shown().data {
        frame.render_widget(members_table(cluster, screen.choice), table);
    }
    draw_panel(frame, screen, body);
    frame.render_widget(Paragraph::new(Span::styled(HELP, Style::new().add_modifier(Modifier::DIM))), foot);
}

/// The table of members: a row for all of them, then one for each. `selected` is 0 for all, else
/// the member's position plus one.
fn members_table(cluster: &ClusterView, selected: usize) -> Table<'static> {
    let header = Row::new(["node", "role", "ready", "auth/s", "acct/s", "discarded/s", "in progress", "address"]).style(Style::new().add_modifier(Modifier::BOLD));
    let sum = crate::status::total(cluster);
    let mark = |index: usize| if index == selected { ">" } else { " " };
    let styled = |index: usize, row: Row<'static>| if index == selected { row.style(Style::new().add_modifier(Modifier::REVERSED)) } else { row };
    let mut rows: Vec<Row> = vec![styled(
        0,
        Row::new([
            format!("{}all", mark(0)),
            String::new(),
            format!("{}/{}", sum.reachable, sum.members),
            format!("{:.1}", sum.figures.auth),
            format!("{:.1}", sum.figures.acct),
            format!("{:.1}", sum.figures.discarded),
            number(sum.figures.in_progress),
            String::new(),
        ]),
    )];
    for (index, member) in cluster.members.iter().enumerate() {
        let (ready, numbers) = match &member.shown {
            Ok(shown) => {
                let figures = crate::status::figures(shown);
                (shown.ready.text.clone(), [format!("{:.1}", figures.auth), format!("{:.1}", figures.acct), format!("{:.1}", figures.discarded), number(figures.in_progress)])
            }
            Err(_) => ("unreachable".to_string(), ["-", "-", "-", "-"].map(str::to_string)),
        };
        let row = Row::new([format!("{}{}", mark(index + 1), member.id), member.role.to_string(), ready, numbers[0].clone(), numbers[1].clone(), numbers[2].clone(), numbers[3].clone(), member.address.clone()]);
        rows.push(styled(index + 1, row));
    }
    let widths = [
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(12),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Min(10),
    ];
    Table::new(rows, widths).header(header)
}

/// The vertical axis of a chart: from 0 to the largest rate, at least 1.
fn y_top(rates: &[f64]) -> f64 {
    rates.iter().copied().fold(1.0, f64::max)
}

/// The panel of the selected section, or the reason there is none.
fn draw_panel(frame: &mut Frame, screen: &Screen, area: ratatui::layout::Rect) {
    let block = |title: String| Block::new().borders(Borders::ALL).title(title);
    let text = match screen.selected() {
        None => Some("no members".to_string()),
        Some(Err(e)) => Some(match &screen.shown().data {
            Data::Cluster(cluster) => cluster.members.get(screen.choice.wrapping_sub(1)).map_or(e.clone(), |member| format!("node {} did not answer: {e}", member.id)),
            Data::Node(_) => e.clone(),
        }),
        Some(Ok(_)) => None,
    };
    if let Some(text) = text {
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(block(String::new())), area);
        return;
    }
    let (Some(Ok(view)), Some(section)) = (screen.selected(), screen.section()) else {
        frame.render_widget(Paragraph::new("no sections").block(block(String::new())), area);
        return;
    };
    let outer = block(format!(" {} ({}/{}) ", section.title, screen.panel + 1, view.sections.len()));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let reset = u16::from(view.counter_reset);
    let rows = u16::try_from(section.items.len() + 1).unwrap_or(u16::MAX);
    let [note, items, chart] = Layout::vertical([Constraint::Length(reset), Constraint::Length(rows), Constraint::Min(0)]).areas(inner);
    if view.counter_reset {
        frame.render_widget(Paragraph::new(Line::from("counters reset since the previous sample: the server restarted")), note);
    }
    let selected = screen.item_index();
    let table_rows: Vec<Row> = section
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let (value, total) = match &item.reading {
                Reading::Rate { rate, total } => (format!("{rate:.1}/s"), number(*total)),
                other => (reading_text(other), String::new()),
            };
            let row = Row::new([item.label.clone(), value, total]);
            if index == selected {
                row.style(Style::new().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect();
    let widths = [Constraint::Min(16), Constraint::Length(16), Constraint::Length(14)];
    let header = Row::new(["", "rate or value", "total"]).style(Style::new().add_modifier(Modifier::DIM));
    frame.render_widget(Table::new(table_rows, widths).header(header), items);
    let Some(item) = section.items.get(selected) else { return };
    if !matches!(item.reading, Reading::Rate { .. }) {
        return;
    }
    let rates: Vec<f64> = screen.shown().history.get(&screen.who(), &section.key, &item.key).map(|kept| kept.iter().copied().collect()).unwrap_or_default();
    let last = rates.len().saturating_sub(1);
    let points: Vec<(f64, f64)> = rates.iter().enumerate().map(|(index, rate)| (-((last - index) as f64), *rate)).collect();
    let top = y_top(&rates);
    let dataset = Dataset::default().marker(Marker::Braille).graph_type(GraphType::Line).data(&points);
    let chart_widget = Chart::new(vec![dataset])
        .block(Block::new().borders(Borders::TOP).title(format!(" {} per second, last {WINDOW} s ", item.label)))
        .x_axis(Axis::default().bounds([-(WINDOW as f64), 0.0]).labels(vec![format!("-{WINDOW} s"), "now".to_string()]))
        .y_axis(Axis::default().bounds([0.0, top]).labels(vec!["0".to_string(), number(top)]));
    frame.render_widget(chart_widget, chart);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{MemberView, Ready, Reading, ShownItem, ShownSection, Value};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    type Test = Result<(), Box<dyn std::error::Error>>;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn section(key: &str, title: &str, items: &[(&str, f64)]) -> ShownSection {
        ShownSection {
            key: key.to_string(),
            title: title.to_string(),
            items: items.iter().map(|(label, value)| ShownItem { key: label.to_string(), label: label.to_string(), reading: Reading::Value(Value::Number(*value)) }).collect(),
        }
    }

    fn view(sections: Vec<ShownSection>) -> Result<View, Box<dyn std::error::Error>> {
        Ok(View { address: "127.0.0.1:9812".parse()?, interval: 1.0, ready: Ready { status: 200, text: "ready".to_string() }, counter_reset: false, sections })
    }

    fn three() -> Result<View, Box<dyn std::error::Error>> {
        view(vec![section("node", "Node", &[("uptime", 5.0)]), section("radius", "RADIUS", &[("auth_requests", 7.0)]), section("tacacs", "TACACS+", &[("requests", 3.0)])])
    }

    /// What the screen draws on a terminal of the given size, one string for the whole screen.
    fn drawn(screen: &Screen, width: u16, height: u16) -> Result<String, Box<dyn std::error::Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| draw(frame, screen))?;
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        Ok(out)
    }

    fn member(id: u64, role: &'static str, shown: Result<View, String>) -> MemberView {
        MemberView { id, role, address: format!("10.0.0.{id}:9812"), shown }
    }

    fn cluster() -> Result<ClusterView, Box<dyn std::error::Error>> {
        Ok(ClusterView {
            from: crate::cluster_commands::Source::Cluster,
            interval: 1.0,
            members: vec![member(1, "leader", Ok(three()?)), member(2, "follower", Ok(view(vec![section("node", "Node", &[("uptime", 9.0)])])?)), member(3, "follower", Err("connection refused".to_string()))],
        })
    }

    #[test]
    fn panels_change_and_wrap() -> Test {
        let mut screen = Screen::new(Data::Node(three().map_err(|e| e.to_string())));
        assert_eq!(screen.panel(), 0);
        screen.on_key(key(KeyCode::Right));
        screen.on_key(key(KeyCode::Right));
        assert_eq!(screen.panel(), 2);
        assert!(drawn(&screen, 60, 10)?.contains("TACACS+ (3/3)"));
        screen.on_key(key(KeyCode::Right));
        assert_eq!(screen.panel(), 0, "the order wraps");
        screen.on_key(key(KeyCode::Left));
        assert_eq!(screen.panel(), 2, "Left goes the other way");
        screen.on_key(key(KeyCode::Tab));
        assert_eq!(screen.panel(), 0);
        Ok(())
    }

    /// Dropping the guard where there is no terminal to restore is harmless.
    #[test]
    fn the_guard_can_be_dropped_without_a_terminal() {
        drop(Restore);
    }

    #[test]
    fn leaving_keys() {
        let mut screen = Screen::new(Data::Node(Err("waiting".to_string())));
        assert_eq!(screen.on_key(key(KeyCode::Char('q'))), Step::Quit);
        assert_eq!(screen.on_key(key(KeyCode::Esc)), Step::Quit);
        assert_eq!(screen.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Step::Quit);
        assert_eq!(screen.on_key(key(KeyCode::Char('c'))), Step::Stay);
        assert_eq!(screen.on_key(key(KeyCode::Right)), Step::Stay, "no panels and no crash");
    }

    #[test]
    fn selecting_a_member_changes_the_panels() -> Test {
        let mut screen = Screen::new(Data::Cluster(cluster()?));
        assert_eq!(screen.choice(), 0, "all is selected at the start");
        screen.on_key(key(KeyCode::Right));
        screen.on_key(key(KeyCode::Right));
        assert_eq!(screen.panel(), 2);
        screen.on_key(key(KeyCode::Char(']')));
        assert_eq!(screen.choice(), 1);
        screen.on_key(key(KeyCode::Char(']')));
        assert_eq!(screen.choice(), 2, "the second member");
        assert_eq!(screen.panel(), 0, "the panel is kept inside the new member's sections");
        let text = drawn(&screen, 100, 16)?;
        assert!(text.contains("Node (1/1)") && text.contains("uptime"), "{text}");
        screen.on_key(key(KeyCode::Char(']')));
        screen.on_key(key(KeyCode::Char(']')));
        assert_eq!(screen.choice(), 3, "] stays on the last member");
        for _ in 0..5 {
            screen.on_key(key(KeyCode::Char('[')));
        }
        assert_eq!(screen.choice(), 0, "[ goes back to all and stays");
        Ok(())
    }

    #[test]
    fn a_member_out_of_reach_is_shown() -> Test {
        let mut screen = Screen::new(Data::Cluster(cluster()?));
        let text = drawn(&screen, 100, 16)?;
        assert!(text.contains("unreachable"), "{text}");
        assert!(text.contains("2/3"), "{text}");
        for _ in 0..3 {
            screen.on_key(key(KeyCode::Char(']')));
        }
        let text = drawn(&screen, 100, 16)?;
        assert!(text.contains("node 3 did not answer: connection refused"), "{text}");
        Ok(())
    }

    #[test]
    fn a_section_of_an_extension_is_a_panel() -> Test {
        let mut sections = three()?.sections;
        sections.push(section("radsec", "RadSec", &[("connections", 4.0)]));
        let mut screen = Screen::new(Data::Node(view(sections).map_err(|e| e.to_string())));
        for _ in 0..3 {
            screen.on_key(key(KeyCode::Right));
        }
        let text = drawn(&screen, 60, 10)?;
        assert!(text.contains("RadSec (4/4)") && text.contains("connections") && text.contains('4'), "{text}");
        Ok(())
    }

    #[test]
    fn new_data_keeps_the_selection_where_it_still_exists() -> Test {
        let mut screen = Screen::new(Data::Cluster(cluster()?));
        screen.on_key(key(KeyCode::Char(']')));
        screen.on_key(key(KeyCode::Char(']')));
        screen.on_key(key(KeyCode::Char(']')));
        assert_eq!(screen.choice(), 3);
        let mut smaller = cluster()?;
        smaller.members.truncate(1);
        screen.set_data(Data::Cluster(smaller));
        assert_eq!(screen.choice(), 1, "the last member that is left");
        Ok(())
    }

    #[test]
    fn a_small_terminal_does_not_panic() -> Test {
        let screen = Screen::new(Data::Cluster(cluster()?));
        for (width, height) in [(1, 1), (10, 3), (40, 5), (200, 60)] {
            drawn(&screen, width, height)?;
        }
        Ok(())
    }

    #[test]
    fn rates_and_text_values_are_drawn_as_status_prints_them() -> Test {
        let mut shown = three()?;
        shown.sections[1].items = vec![
            ShownItem { key: "r".to_string(), label: "rate".to_string(), reading: Reading::Rate { rate: 12.5, total: 100.0 } },
            ShownItem { key: "t".to_string(), label: "state".to_string(), reading: Reading::Value(Value::Text("fine".to_string())) },
        ];
        let mut screen = Screen::new(Data::Node(Ok(shown)));
        screen.on_key(key(KeyCode::Right));
        let text = drawn(&screen, 60, 10)?;
        assert!(text.contains("12.5/s") && text.contains("100") && text.contains("fine"), "{text}");
        Ok(())
    }

    fn rated(key: &str, title: &str, items: &[(&str, f64)]) -> ShownSection {
        ShownSection {
            key: key.to_string(),
            title: title.to_string(),
            items: items.iter().map(|(name, rate)| ShownItem { key: name.to_string(), label: name.to_string(), reading: Reading::Rate { rate: *rate, total: rate * 10.0 } }).collect(),
        }
    }

    fn node_with(rate_of: f64) -> Result<View, String> {
        view(vec![rated("radius", "RADIUS", &[("auth", rate_of), ("acct", rate_of + 1.0), ("accept", 2.0)]), section("node", "Node", &[("uptime", 5.0)])]).map_err(|e| e.to_string())
    }

    fn rates_of(screen: &Screen, who: &str, section: &str, item: &str) -> Vec<f64> {
        screen.shown().history.get(who, section, item).map(|kept| kept.iter().copied().collect()).unwrap_or_default()
    }

    #[test]
    fn the_chart_fills_as_time_passes() {
        let mut screen = Screen::new(Data::Node(node_with(0.0)));
        for second in 1..5 {
            screen.set_data(Data::Node(node_with(f64::from(second))));
        }
        assert_eq!(rates_of(&screen, "node", "radius", "auth").len(), 5, "five samples, five points");
        for second in 5..70 {
            screen.set_data(Data::Node(node_with(f64::from(second))));
        }
        let kept = rates_of(&screen, "node", "radius", "auth");
        assert_eq!(kept.len(), 60);
        assert_eq!((kept.first().copied(), kept.last().copied()), (Some(10.0), Some(69.0)), "the last 60 samples, the newest last");
        assert!(rates_of(&screen, "node", "node", "uptime").is_empty(), "nothing is kept for an item without a rate");
    }

    #[test]
    fn a_restart_is_a_rate_of_zero() -> Test {
        let mut screen = Screen::new(Data::Node(node_with(7.0)));
        let mut reset = view(vec![rated("radius", "RADIUS", &[("auth", 900.0)])])?;
        reset.counter_reset = true;
        screen.set_data(Data::Node(Ok(reset)));
        screen.set_data(Data::Node(node_with(5.0)));
        assert_eq!(rates_of(&screen, "node", "radius", "auth"), [7.0, 0.0, 5.0]);
        Ok(())
    }

    #[test]
    fn all_the_members_that_answered_are_added_up() -> Test {
        let data = |a: f64, b: f64| -> Result<Data, Box<dyn std::error::Error>> {
            Ok(Data::Cluster(ClusterView {
                from: crate::cluster_commands::Source::Cluster,
                interval: 1.0,
                members: vec![member(1, "leader", Ok(view(vec![rated("radius", "RADIUS", &[("auth", a)])])?)), member(2, "follower", Ok(view(vec![rated("radius", "RADIUS", &[("auth", b)])])?)), member(3, "follower", Err("connection refused".to_string()))],
            }))
        };
        let mut screen = Screen::new(data(100.0, 50.0)?);
        screen.on_key(key(KeyCode::Right));
        assert_eq!(rates_of(&screen, "all", "radius", "auth"), [150.0]);
        assert_eq!(rates_of(&screen, "1", "radius", "auth"), [100.0]);
        assert_eq!(rates_of(&screen, "2", "radius", "auth"), [50.0]);
        let text = drawn(&screen, 100, 24)?;
        assert!(text.contains("150.0/s") && text.contains("connection refused") || text.contains("unreachable"), "{text}");
        assert!(text.contains("150.0/s"), "{text}");
        Ok(())
    }

    #[test]
    fn items_are_selected_by_key_and_the_chart_follows() -> Test {
        let mut screen = Screen::new(Data::Node(node_with(3.5)));
        assert_eq!(screen.item_key(), Some("auth"));
        screen.set_data(Data::Node(node_with(12.25)));
        let text = drawn(&screen, 80, 24)?;
        assert!(text.contains("auth per second"), "{text}");
        screen.on_key(key(KeyCode::Down));
        assert_eq!(screen.item_key(), Some("acct"));
        let text = drawn(&screen, 80, 24)?;
        assert!(text.contains("acct per second") && !text.contains("auth per second"), "{text}");
        screen.on_key(key(KeyCode::Down));
        screen.on_key(key(KeyCode::Down));
        assert_eq!(screen.item_key(), Some("accept"), "Down stays on the last item");
        screen.on_key(key(KeyCode::Up));
        screen.on_key(key(KeyCode::Up));
        screen.on_key(key(KeyCode::Up));
        assert_eq!(screen.item_key(), Some("auth"));
        // The item is gone from the next sample: back to the first item.
        screen.on_key(key(KeyCode::Down));
        screen.set_data(Data::Node(view(vec![rated("radius", "RADIUS", &[("auth", 1.0)])]).map_err(|e| e.to_string())));
        assert_eq!(screen.item_key(), Some("auth"));
        Ok(())
    }

    #[test]
    fn the_vertical_axis_runs_from_zero_to_the_largest_rate_but_at_least_one() -> Test {
        assert_eq!(y_top(&[0.0, 3.5, 12.25]), 12.25);
        assert_eq!(y_top(&[0.0]), 1.0);
        assert_eq!(y_top(&[]), 1.0);
        let mut screen = Screen::new(Data::Node(node_with(0.0)));
        screen.set_data(Data::Node(node_with(12.25)));
        let text = drawn(&screen, 80, 24)?;
        assert!(text.contains("12.25"), "{text}");
        Ok(())
    }

    #[test]
    fn an_item_without_a_rate_has_no_chart() -> Test {
        let mut screen = Screen::new(Data::Node(node_with(1.0)));
        screen.on_key(key(KeyCode::Right));
        let text = drawn(&screen, 80, 24)?;
        assert!(text.contains("uptime") && !text.contains("per second"), "{text}");
        Ok(())
    }

    #[test]
    fn pausing_freezes_the_screen_but_not_the_rates() -> Test {
        let mut screen = Screen::new(Data::Node(node_with(1.0)));
        let before = drawn(&screen, 80, 24)?;
        screen.on_key(key(KeyCode::Char('p')));
        screen.set_data(Data::Node(node_with(30.0)));
        screen.set_data(Data::Node(node_with(40.0)));
        let paused = drawn(&screen, 80, 24)?;
        assert!(paused.contains("(paused)"), "{paused}");
        let body = |text: &str| text.lines().skip(1).map(str::to_string).collect::<Vec<_>>();
        assert_eq!(body(&paused), body(&before), "nothing changed in between");
        assert_eq!(screen.on_key(key(KeyCode::Char('p'))), Step::Stay);
        assert_eq!(rates_of(&screen, "node", "radius", "auth"), [1.0, 30.0, 40.0], "the chart has the two samples that came in meanwhile");
        let after = drawn(&screen, 80, 24)?;
        assert!(after.contains("40.0/s") && !after.contains("(paused)"), "{after}");
        Ok(())
    }

    #[test]
    fn the_help_line_fits_in_80_columns() -> Test {
        for data in [Data::Node(node_with(1.0)), Data::Cluster(cluster()?)] {
            let text = drawn(&Screen::new(data), 80, 24)?;
            assert!(text.contains("p pause") && text.contains("q quit") && text.contains("[ ] member"), "{text}");
        }
        Ok(())
    }
}
