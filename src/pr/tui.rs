use crate::error::Result;
use crate::package::Package;
use crate::pr::bump::Bump;
use crate::pr::run::UpdatedCrate;
use crate::pr::traits::CommitInfo;
use anyhow::Context;
use cargo_metadata::semver::Version;
use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, StatefulWidget, Widget, Wrap};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Continue,
    Confirm,
    Cancel,
}

/// A changed package plus the version bump options the user can pick between,
/// as shown in one row of the TUI's package list.
pub struct PackageItem {
    package: Package,
    commits: Vec<CommitInfo>,
    patch: Version,
    minor: Version,
    major: Version,
    selected: Option<Bump>,
}

impl PackageItem {
    /// `baseline` is the version before this branch's first still-unmerged bump (or the
    /// package's own current version, if it has none) — the patch/minor/major options are
    /// computed from there, not from `package`'s current version, so a package that already has
    /// a staged bump can show its current version as a still-selectable option (e.g. `patch`
    /// landing back on the version it's already at) rather than always offering one past it. See
    /// `Bump::apply_to`.
    ///
    /// Each option is clamped to at least `package`'s current version: if a bigger bump is
    /// already staged (e.g. a minor bump already landed the package on 1.3.0 from a 1.2.5
    /// baseline), a lower-tier option like `patch` (which would otherwise compute 1.2.6) must not
    /// offer what would be a downgrade — it should show the same already-covering 1.3.0 instead.
    ///
    /// Whenever something is already staged (`current > baseline`), `patch` is preselected — not
    /// because the staged bump was necessarily patch-level, but because `patch` always clamps to
    /// exactly `current` in that state (see the proof below), so preselecting it *is* preselecting
    /// "stay at the current version". The realistic choice is then "stay put" vs. "escalate to a
    /// higher tier", not picking from scratch. A package with no prior bump (`current == baseline`)
    /// has nothing to stay at, so it's left unselected as before.
    ///
    /// Why `patch` always clamps to `current` there: `Bump::Patch.apply_to(baseline)` only differs
    /// from `baseline` in the patch field, so it's less than `current` in every field that already
    /// differs from `baseline` (major, minor, or a patch increase of more than one — the last
    /// covers a package whose current version reflects several stacked bumps from before this
    /// baseline/poaching scheme existed), and `max(_, current)` picks `current` in all of them.
    pub fn new(package: Package, commits: Vec<CommitInfo>, baseline: &Version) -> Self {
        let current = package.version.clone();
        let patch = Bump::Patch.apply_to(baseline).max(current.clone());
        let minor = Bump::Minor.apply_to(baseline).max(current.clone());
        let major = Bump::Major.apply_to(baseline).max(current.clone());
        let selected = (current > *baseline).then_some(Bump::Patch);
        Self {
            package,
            commits,
            patch,
            minor,
            major,
            selected,
        }
    }

    pub const fn package(&self) -> &Package {
        &self.package
    }

    const fn version_for(&self, bump: Bump) -> &Version {
        match bump {
            Bump::Patch => &self.patch,
            Bump::Minor => &self.minor,
            Bump::Major => &self.major,
        }
    }

    // "{name}  {old} -> {new}" once a bump is picked, else "{name}  {old} -> ?"
    // — shown on the package list so every row's bump is visible at a glance.
    fn row_label(&self) -> String {
        let current = &self.package.version;
        let target = self.selected.map_or_else(
            || "?".to_string(),
            |bump| self.version_for(bump).to_string(),
        );
        format!("{}  {current} -> {target}", self.package.name)
    }

    // Red until a bump is picked, green once it is — the row's own color is
    // the selection indicator, so it reads at a glance without a separate
    // mark (and stays legible even on the focused/reverse-video row).
    const fn row_style(&self) -> Style {
        let color = if self.selected.is_some() {
            Color::Green
        } else {
            Color::Red
        };
        Style::new().fg(color)
    }
}

struct App {
    packages: Vec<PackageItem>,
    focused: usize,
    commit_scroll: u16,
}

impl App {
    // vim-style Ctrl-d/Ctrl-u half-page scroll step for the commit list.
    const SCROLL_STEP: u16 = 10;

    const fn new(packages: Vec<PackageItem>) -> Self {
        Self {
            packages,
            focused: 0,
            commit_scroll: 0,
        }
    }

    const fn focus_up(&mut self) {
        self.focused = self.focused.saturating_sub(1);
        self.commit_scroll = 0;
    }

    const fn focus_down(&mut self) {
        if self.focused + 1 < self.packages.len() {
            self.focused += 1;
        }
        self.commit_scroll = 0;
    }

    fn select(&mut self, bump: Bump) {
        if let Some(pkg) = self.packages.get_mut(self.focused) {
            pkg.selected = Some(bump);
        }
    }

    fn clear_focused(&mut self) {
        if let Some(pkg) = self.packages.get_mut(self.focused) {
            pkg.selected = None;
        }
    }

    fn all_selected(&self) -> bool {
        self.packages.iter().all(|p| p.selected.is_some())
    }

    fn selected_count(&self) -> usize {
        self.packages
            .iter()
            .filter(|p| p.selected.is_some())
            .count()
    }

    fn handle_key(&mut self, key: KeyEvent) -> Signal {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('k') => self.focus_up(),
            KeyCode::Char('j') => self.focus_down(),
            KeyCode::Char('1') => self.select(Bump::Patch),
            KeyCode::Char('2') => self.select(Bump::Minor),
            KeyCode::Char('3') => self.select(Bump::Major),
            // vim-style Ctrl-u/Ctrl-d scroll the focused package's commits.
            KeyCode::Char('u') if ctrl => {
                self.commit_scroll = self.commit_scroll.saturating_sub(Self::SCROLL_STEP);
            }
            KeyCode::Char('d') if ctrl => {
                self.commit_scroll = self.commit_scroll.saturating_add(Self::SCROLL_STEP);
            }
            KeyCode::Char('d') | KeyCode::Backspace | KeyCode::Delete => self.clear_focused(),
            KeyCode::Char('c') if self.all_selected() => {
                return Signal::Confirm;
            }
            KeyCode::Char('q') | KeyCode::Esc => return Signal::Cancel,
            _ => {}
        }
        Signal::Continue
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<Signal> {
        loop {
            terminal
                .draw(|frame| frame.render_widget(&*self, frame.area()))
                .context("draw tui frame")?;

            if let Event::Key(key) = event::read().context("read terminal event")? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match self.handle_key(key) {
                    Signal::Continue => {}
                    signal => return Ok(signal),
                }
            }
        }
    }

    fn into_updated_crates(self) -> Vec<UpdatedCrate> {
        self.packages
            .into_iter()
            .filter_map(|pkg| {
                let bump = pkg.selected?;
                let new_version = pkg.version_for(bump).clone();
                Some(UpdatedCrate {
                    package: pkg.package,
                    new_version,
                    commits: pkg.commits,
                })
            })
            .collect()
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [main, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
                .areas(main);

        self.render_package_list(left, buf);
        self.render_details(right, buf);
        self.render_footer(footer, buf);
    }
}

impl App {
    fn render_package_list(&self, area: Rect, buf: &mut Buffer) {
        let items: Vec<ListItem> = self
            .packages
            .iter()
            .map(|pkg| ListItem::new(pkg.row_label()).style(pkg.row_style()))
            .collect();

        // Bold (not reversed) so focus doesn't invert the row's red/green
        // fg color into a solid highlight block — the text stays red/green,
        // just bolder, when focused.
        let list = List::new(items)
            .block(Block::bordered().title("Packages"))
            .highlight_style(Style::new().add_modifier(Modifier::BOLD));

        let mut state = ListState::default().with_selected(Some(self.focused));
        StatefulWidget::render(list, area, buf, &mut state);
    }

    fn render_details(&self, area: Rect, buf: &mut Buffer) {
        let [info, commits] =
            Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(area);
        let Some(pkg) = self.packages.get(self.focused) else {
            return;
        };

        let mut lines = vec![Line::from(format!(
            "{}  (current {})",
            pkg.package.name, pkg.package.version
        ))];
        for bump in Bump::ALL {
            let marker = if pkg.selected == Some(bump) {
                "[x]"
            } else {
                "[ ]"
            };
            lines.push(Line::from(format!(
                "{marker} {} -> {}",
                bump.label(),
                pkg.version_for(bump)
            )));
        }
        Paragraph::new(lines)
            .block(Block::bordered().title("Version bump"))
            .render(info, buf);

        let commit_lines: Vec<Line> = if pkg.commits.is_empty() {
            vec![Line::from("(no attributed commits)")]
        } else {
            pkg.commits
                .iter()
                .map(|c| Line::from(format!("{} {}", c.short_id(), c.summary)))
                .collect()
        };
        Paragraph::new(commit_lines)
            .block(Block::bordered().title("Commits"))
            .wrap(Wrap { trim: false })
            .scroll((self.commit_scroll, 0))
            .render(commits, buf);
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        let hints = " j/k move  1/2/3 bump  d clear  ctrl-d/ctrl-u scroll  c confirm  q quit";
        let status = format!(
            "{}/{} selected ",
            self.selected_count(),
            self.packages.len()
        );

        let status_width = u16::try_from(status.chars().count()).unwrap_or(u16::MAX);
        let [left, right] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(status_width)]).areas(area);

        Paragraph::new(hints).render(left, buf);
        Paragraph::new(status).render(right, buf);
    }
}

/// Runs the interactive bump-selection TUI over `changed`, which pairs each package with its
/// baseline (see `PackageItem::new`) alongside its commits. Returns `None` if the user cancelled
/// (nothing should be written to disk), or the confirmed selections otherwise.
pub fn run(
    changed: HashMap<Package, (Version, Vec<CommitInfo>)>,
) -> Result<Option<Vec<UpdatedCrate>>> {
    let mut packages: Vec<PackageItem> = changed
        .into_iter()
        .map(|(package, (baseline, commits))| PackageItem::new(package, commits, &baseline))
        .collect();
    packages.sort_by(|a, b| a.package().name.cmp(&b.package().name));

    let mut app = App::new(packages);

    let mut terminal = ratatui::init();
    let outcome = app.run(&mut terminal);
    ratatui::restore();

    match outcome? {
        Signal::Confirm => Ok(Some(app.into_updated_crates())),
        Signal::Cancel => Ok(None),
        Signal::Continue => unreachable!("App::run only returns on Confirm or Cancel"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, version: &str) -> PackageItem {
        let version = Version::parse(version).unwrap();
        PackageItem::new(
            Package {
                path: name.to_string(),
                name: name.to_string(),
                version: version.clone(),
            },
            Vec::new(),
            &version,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, event::KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, event::KeyModifiers::CONTROL)
    }

    fn app(names_and_versions: &[(&str, &str)]) -> App {
        App::new(
            names_and_versions
                .iter()
                .map(|(n, v)| package(n, v))
                .collect(),
        )
    }

    #[test]
    fn navigation_clamps_at_bounds() {
        let mut app = app(&[("a", "0.1.0"), ("b", "0.1.0")]);
        assert_eq!(app.handle_key(key(KeyCode::Char('k'))), Signal::Continue);
        assert_eq!(app.focused, 0, "can't move above the first package");

        assert_eq!(app.handle_key(key(KeyCode::Char('j'))), Signal::Continue);
        assert_eq!(app.focused, 1);

        assert_eq!(app.handle_key(key(KeyCode::Char('j'))), Signal::Continue);
        assert_eq!(app.focused, 1, "can't move past the last package");
    }

    #[test]
    fn selecting_a_bump_sets_the_right_version() {
        let mut app = app(&[("a", "0.1.0")]);
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.packages[0].selected, Some(Bump::Minor));
    }

    #[test]
    fn reselecting_a_different_bump_overwrites_the_choice() {
        let mut app = app(&[("a", "0.1.0")]);
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.packages[0].selected, Some(Bump::Major));
    }

    #[test]
    fn d_and_backspace_and_delete_all_clear_the_focused_selection() {
        for code in [KeyCode::Char('d'), KeyCode::Backspace, KeyCode::Delete] {
            let mut app = app(&[("a", "0.1.0")]);
            app.handle_key(key(KeyCode::Char('1')));
            app.handle_key(key(code));
            assert_eq!(app.packages[0].selected, None);
        }
    }

    #[test]
    fn ctrl_d_and_ctrl_u_scroll_the_commit_list() {
        let mut app = app(&[("a", "0.1.0")]);
        app.handle_key(ctrl_key(KeyCode::Char('d')));
        assert_eq!(app.commit_scroll, App::SCROLL_STEP);

        app.handle_key(ctrl_key(KeyCode::Char('u')));
        assert_eq!(app.commit_scroll, 0);

        // clamps at zero rather than underflowing
        app.handle_key(ctrl_key(KeyCode::Char('u')));
        assert_eq!(app.commit_scroll, 0);
    }

    #[test]
    fn plain_d_clears_rather_than_scrolling() {
        let mut app = app(&[("a", "0.1.0")]);
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.packages[0].selected, None);
        assert_eq!(app.commit_scroll, 0);
    }

    #[test]
    fn confirm_requires_every_package_to_have_a_selection() {
        let mut app = app(&[("a", "0.1.0"), ("b", "0.1.0")]);
        assert_eq!(app.handle_key(key(KeyCode::Char('c'))), Signal::Continue);

        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.handle_key(key(KeyCode::Char('c'))), Signal::Continue);

        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.handle_key(key(KeyCode::Char('c'))), Signal::Confirm);
    }

    #[test]
    fn quit_or_escape_always_cancels() {
        let mut app = app(&[("a", "0.1.0")]);
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Signal::Cancel);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Signal::Cancel);
    }

    #[test]
    fn into_updated_crates_uses_the_selected_version() {
        let mut app = app(&[("a", "0.1.0")]);
        app.handle_key(key(KeyCode::Char('3')));
        let updated = app.into_updated_crates();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].new_version, Version::parse("1.0.0").unwrap());
    }

    #[test]
    fn bump_options_are_computed_from_the_baseline_not_the_current_version() {
        // package currently sits at 1.3.0 (already staged by a prior, still-unmerged bump), but
        // its baseline — before that bump — was 1.2.5. The patch option should land back on 1.3.0
        // (the already-staged minor bump), letting the user "change their mind" and pick the
        // version that's already current, rather than always offering one past it.
        let current = Version::parse("1.3.0").unwrap();
        let baseline = Version::parse("1.2.5").unwrap();
        let item = PackageItem::new(
            Package {
                path: "a".to_string(),
                name: "a".to_string(),
                version: current.clone(),
            },
            Vec::new(),
            &baseline,
        );

        assert_eq!(*item.version_for(Bump::Patch), current);
        assert_eq!(*item.version_for(Bump::Minor), current);
        assert_eq!(
            *item.version_for(Bump::Major),
            Version::parse("2.0.0").unwrap()
        );
    }

    #[test]
    fn a_package_with_something_already_staged_defaults_to_staying_at_the_current_version() {
        // baseline 1.2.5 -> current 1.3.0 (a minor bump already landed). `patch` comes
        // preselected — not because the staged bump was patch-level, but because it clamps to
        // exactly 1.3.0 here, so preselecting it means "stay at the current version". The
        // realistic choice is then "stay put" vs. "escalate", not picking from scratch.
        let item = PackageItem::new(
            Package {
                path: "a".to_string(),
                name: "a".to_string(),
                version: Version::parse("1.3.0").unwrap(),
            },
            Vec::new(),
            &Version::parse("1.2.5").unwrap(),
        );
        assert_eq!(item.selected, Some(Bump::Patch));
        assert_eq!(
            *item.version_for(Bump::Patch),
            Version::parse("1.3.0").unwrap()
        );
    }

    #[test]
    fn a_package_with_several_stacked_legacy_bumps_still_defaults_to_the_current_version() {
        // baseline 1.2.5 -> current 1.2.7: two patch-level bumps stacked on top of each other from
        // before this baseline/poaching scheme existed, rather than one clean bump. `patch` must
        // still clamp to (and preselect) the real current version, 1.2.7, not the raw 1.2.6 a
        // single patch off the baseline would give.
        let item = PackageItem::new(
            Package {
                path: "a".to_string(),
                name: "a".to_string(),
                version: Version::parse("1.2.7").unwrap(),
            },
            Vec::new(),
            &Version::parse("1.2.5").unwrap(),
        );
        assert_eq!(item.selected, Some(Bump::Patch));
        assert_eq!(
            *item.version_for(Bump::Patch),
            Version::parse("1.2.7").unwrap()
        );
    }

    #[test]
    fn a_package_with_no_prior_bump_starts_unselected() {
        // baseline == current: no tier is a no-op, so nothing already staged to preselect.
        let item = package("a", "0.1.0");
        assert_eq!(item.selected, None);
    }

    #[test]
    fn preselection_lets_confirm_succeed_without_touching_an_unescalated_package() {
        let mut app = App::new(vec![PackageItem::new(
            Package {
                path: "a".to_string(),
                name: "a".to_string(),
                version: Version::parse("1.3.0").unwrap(),
            },
            Vec::new(),
            &Version::parse("1.2.5").unwrap(),
        )]);
        assert_eq!(app.handle_key(key(KeyCode::Char('c'))), Signal::Confirm);
    }
}
