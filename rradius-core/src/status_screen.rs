//! The full-screen `status --tui`: what `status` prints, drawn on a terminal and changed with keys.
//!
//! The screen draws the [`View`] or [`ClusterView`] that `status` builds for its text and JSON
//! outputs, so a section an extension adds is a panel here without a change to this module.

use crate::status::{figures, number, reading_text, total, ClusterView, Data, View};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

/// What a key asks of the loop that owns the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Stay,
    Quit,
}

/// The data and what is selected in it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Screen {
    data: Data,
    panel: usize,
    member: usize,
}

impl Screen {
    pub(crate) fn new(data: Data) -> Self {
        Screen { data, panel: 0, member: 0 }
    }

    /// Shows newer data and keeps the selection where it still exists.
    pub(crate) fn set_data(&mut self, data: Data) {
        self.data = data;
        if let Data::Cluster(cluster) = &self.data {
            self.member = self.member.min(cluster.members.len().saturating_sub(1));
        }
        self.panel = self.panel.min(self.panel_count().saturating_sub(1));
    }

    /// The view of the node, or of the selected member, when it has one.
    fn selected(&self) -> Option<&Result<View, String>> {
        match &self.data {
            Data::Node(view) => Some(view),
            Data::Cluster(cluster) => cluster.members.get(self.member).map(|member| &member.shown),
        }
    }

    fn panel_count(&self) -> usize {
        match self.selected() {
            Some(Ok(view)) => view.sections.len(),
            _ => 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn panel(&self) -> usize {
        self.panel
    }

    #[cfg(test)]
    pub(crate) fn member(&self) -> usize {
        self.member
    }

    /// Applies a key.
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Step {
        if key.kind == KeyEventKind::Release {
            return Step::Stay;
        }
        let count = self.panel_count();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Step::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Step::Quit,
            KeyCode::Right | KeyCode::Tab if count > 0 => self.panel = (self.panel + 1) % count,
            KeyCode::Left | KeyCode::BackTab if count > 0 => self.panel = (self.panel + count - 1) % count,
            KeyCode::Down => {
                if let Data::Cluster(cluster) = &self.data {
                    self.member = (self.member + 1).min(cluster.members.len().saturating_sub(1));
                    self.panel = self.panel.min(self.panel_count().saturating_sub(1));
                }
            }
            KeyCode::Up => {
                if matches!(self.data, Data::Cluster(_)) {
                    self.member = self.member.saturating_sub(1);
                    self.panel = self.panel.min(self.panel_count().saturating_sub(1));
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
    let _restore = Restore;
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
            drop(_restore);
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

const HELP: &str = " Left/Right/Tab panel   Up/Down member   q quit";

/// Draws the screen.
pub(crate) fn draw(frame: &mut Frame, screen: &Screen) {
    let members = match &screen.data {
        Data::Cluster(cluster) => u16::try_from(cluster.members.len() + 3).unwrap_or(u16::MAX),
        Data::Node(_) => 0,
    };
    let [head, table, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Length(members), Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let title = match &screen.data {
        Data::Node(Ok(view)) => format!("rradius status of {} over {:.1} s", view.address, view.interval),
        Data::Node(Err(_)) => "rradius status".to_string(),
        Data::Cluster(cluster) => format!("rradius cluster status over {:.1} s", cluster.interval),
    };
    frame.render_widget(Paragraph::new(Span::styled(title, bold)), head);
    if let Data::Cluster(cluster) = &screen.data {
        frame.render_widget(members_table(cluster, screen.member), table);
    }
    frame.render_widget(panel(screen), body);
    frame.render_widget(Paragraph::new(Span::styled(HELP, Style::new().add_modifier(Modifier::DIM))), foot);
}

fn members_table(cluster: &ClusterView, selected: usize) -> Table<'static> {
    let header = Row::new(["node", "role", "ready", "auth/s", "acct/s", "discarded/s", "in progress", "address"]).style(Style::new().add_modifier(Modifier::BOLD));
    let mut rows: Vec<Row> = cluster
        .members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            let (ready, numbers) = match &member.shown {
                Ok(shown) => {
                    let figures = figures(shown);
                    (shown.ready.text.clone(), [format!("{:.1}", figures.auth), format!("{:.1}", figures.acct), format!("{:.1}", figures.discarded), number(figures.in_progress)])
                }
                Err(_) => ("unreachable".to_string(), ["-", "-", "-", "-"].map(str::to_string)),
            };
            let mark = if index == selected { ">" } else { " " };
            let row = Row::new([format!("{mark}{}", member.id), member.role.to_string(), ready, numbers[0].clone(), numbers[1].clone(), numbers[2].clone(), numbers[3].clone(), member.address.clone()]);
            if index == selected {
                row.style(Style::new().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect();
    let sum = total(cluster);
    rows.push(Row::new([
        " total".to_string(),
        String::new(),
        format!("{}/{}", sum.reachable, sum.members),
        format!("{:.1}", sum.figures.auth),
        format!("{:.1}", sum.figures.acct),
        format!("{:.1}", sum.figures.discarded),
        number(sum.figures.in_progress),
        String::new(),
    ]));
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

/// The panel of the selected section, or the reason there is none.
fn panel(screen: &Screen) -> Paragraph<'static> {
    let block = |title: String| Block::new().borders(Borders::ALL).title(title);
    match screen.selected() {
        None => Paragraph::new("no members").block(block(String::new())),
        Some(Err(e)) => {
            let text = match &screen.data {
                Data::Cluster(cluster) => cluster.members.get(screen.member).map_or(e.clone(), |member| format!("node {} did not answer: {e}", member.id)),
                Data::Node(_) => e.clone(),
            };
            Paragraph::new(text).wrap(Wrap { trim: false }).block(block(String::new()))
        }
        Some(Ok(view)) => {
            let Some(section) = view.sections.get(screen.panel) else {
                return Paragraph::new("no sections").block(block(String::new()));
            };
            let width = section.items.iter().map(|item| item.label.chars().count()).max().unwrap_or(0);
            let mut lines: Vec<Line> = Vec::new();
            if view.counter_reset {
                lines.push(Line::from("counters reset since the previous sample: the server restarted"));
                lines.push(Line::from(""));
            }
            for item in &section.items {
                lines.push(Line::from(format!("{:width$}  {}", item.label, reading_text(&item.reading))));
            }
            Paragraph::new(lines).block(block(format!(" {} ({}/{}) ", section.title, screen.panel + 1, view.sections.len())))
        }
    }
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
        screen.on_key(key(KeyCode::Right));
        screen.on_key(key(KeyCode::Right));
        assert_eq!(screen.panel(), 2);
        screen.on_key(key(KeyCode::Down));
        assert_eq!(screen.member(), 1);
        assert_eq!(screen.panel(), 0, "the panel is kept inside the new member's sections");
        let text = drawn(&screen, 100, 14)?;
        assert!(text.contains("Node (1/1)") && text.contains("uptime"), "{text}");
        screen.on_key(key(KeyCode::Down));
        screen.on_key(key(KeyCode::Down));
        assert_eq!(screen.member(), 2, "Down stays on the last member");
        screen.on_key(key(KeyCode::Up));
        screen.on_key(key(KeyCode::Up));
        screen.on_key(key(KeyCode::Up));
        assert_eq!(screen.member(), 0);
        Ok(())
    }

    #[test]
    fn a_member_out_of_reach_is_shown() -> Test {
        let mut screen = Screen::new(Data::Cluster(cluster()?));
        let text = drawn(&screen, 100, 14)?;
        assert!(text.contains("unreachable"), "{text}");
        assert!(text.contains("2/3"), "{text}");
        screen.on_key(key(KeyCode::Down));
        screen.on_key(key(KeyCode::Down));
        let text = drawn(&screen, 100, 14)?;
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
        screen.on_key(key(KeyCode::Down));
        screen.on_key(key(KeyCode::Down));
        let mut smaller = cluster()?;
        smaller.members.truncate(1);
        screen.set_data(Data::Cluster(smaller));
        assert_eq!(screen.member(), 0);
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
        assert!(text.contains("12.5/s (100)") && text.contains("fine"), "{text}");
        Ok(())
    }
}
