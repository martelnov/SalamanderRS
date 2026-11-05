//! Salamander RS in an SSH-friendly terminal. Two front ends, one Rust engine.
//! The GTK front end remains the original `salamander-rs` binary.

mod panel;
mod ui;

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use async_channel::TryRecvError;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Terminal;

use salamander_core::archive::{self, ExtractRequest, JobResult};
use salamander_core::attrs::{self, AttrChange, ModeChange};
use salamander_core::config::Config;
use salamander_core::find::{self, FindEvent, Query};
use salamander_core::model::{self, EntryKind};
use salamander_core::ops::{self, ConflictAction, Op, OpEvent, OpKind};
use salamander_core::remote::{self, askpass::PromptKind, conn::Connection, servers, transfer};
use salamander_core::sodium::Secret;

use crate::panel::Panel;

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Restores a usable shell even after most errors or a Rust panic.
struct Screen {
    term: Term,
}

impl Screen {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(term) => Ok(Self { term }),
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, Show);
                Err(error)
            }
        }
    }

    fn suspend(&mut self, process: &mut Command) -> io::Result<()> {
        let _ = self.term.show_cursor();
        disable_raw_mode()?;
        execute!(self.term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture, Show)?;
        let process_result = process.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status();
        let reenter = execute!(self.term.backend_mut(), EnterAlternateScreen, EnableMouseCapture, Hide);
        let raw = enable_raw_mode();
        self.term.clear()?;
        reenter?;
        raw?;
        process_result.map(|_| ())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture, Show);
    }
}

pub struct App {
    panels: [Panel; 2],
    active: usize,
    status: String,
    quit: bool,
}

impl App {
    fn new(left: PathBuf, right: PathBuf, hidden: bool) -> Result<Self, String> {
        Ok(Self {
            panels: [Panel::new(left, hidden)?, Panel::new(right, hidden)?],
            active: 0,
            status: "Připraveno. F1 nápověda · Ctrl+K SFTP · F9 shell".into(),
            quit: false,
        })
    }

    fn front(&self) -> &Panel { &self.panels[self.active] }
    fn front_mut(&mut self) -> &mut Panel { &mut self.panels[self.active] }
    fn other(&self) -> &Panel { &self.panels[1 - self.active] }

    fn render(&mut self, screen: &mut Screen) -> io::Result<()> {
        let height = screen.term.size()?.height;
        let visible = ui::visible_rows(height);
        for panel in &mut self.panels { panel.ensure_visible(visible); }
        screen.term.draw(|frame| ui::draw(frame, self))?;
        Ok(())
    }

    fn refresh(&mut self) {
        for panel in &mut self.panels {
            if let Err(error) = panel.refresh() {
                self.status = format!("Obnova: {error}");
            }
        }
    }

    fn report_job(&mut self, name: &str, result: &JobResult) {
        self.status = format!("{name}: {} dokončeno, {} chyb{}{}", result.done, result.errors.len(),
            if result.errors.len() == 1 { "a" } else { "" },
            if result.cancelled { ", přerušeno" } else { "" });
        if let Some(error) = result.errors.first() {
            self.status.push_str(&format!(" | {}: {}", error.source.display(), error.error));
        }
    }

    fn show_current(&mut self, screen: &mut Screen) -> io::Result<()> {
        let selected = self.front().current().map(|item| (item.kind, item.path.clone(), item.is_dir_like()));
        let Some((kind, path, directory)) = selected else { return Ok(()); };
        if kind == EntryKind::Parent || directory || archive::looks_like_archive(&path) && self.front().remote().is_none() {
            if let Err(error) = self.front_mut().change_dir(path) {
                self.status = error;
            }
        } else if kind == EntryKind::File || kind == EntryKind::Symlink {
            self.viewer(screen, &path)?;
        }
        Ok(())
    }

    fn handle(&mut self, key: KeyEvent, screen: &mut Screen) -> io::Result<()> {
        if key.kind != KeyEventKind::Press { return Ok(()); }
        let visible = ui::visible_rows(screen.term.size()?.height);
        match (key.code, key.modifiers) {
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => self.active = 1 - self.active,
            (KeyCode::Up, _) => self.front_mut().move_cursor(-1, visible),
            (KeyCode::Down, _) => self.front_mut().move_cursor(1, visible),
            (KeyCode::Home, _) => self.front_mut().move_cursor(-1_000_000, visible),
            (KeyCode::End, _) => self.front_mut().move_cursor(1_000_000, visible),
            (KeyCode::PageUp, _) => self.front_mut().move_cursor(-(visible as isize), visible),
            (KeyCode::PageDown, _) => self.front_mut().move_cursor(visible as isize, visible),
            (KeyCode::Enter, _) => self.show_current(screen)?,
            (KeyCode::Backspace, _) => {
                if let Err(error) = self.front_mut().go_parent() { self.status = error; }
            }
            (KeyCode::Insert, _) | (KeyCode::Char(' '), _) => {
                self.front_mut().toggle_mark();
                self.front_mut().move_cursor(1, visible);
            }
            (KeyCode::F(1), _) => self.help(screen)?,
            (KeyCode::F(2), m) if m.contains(KeyModifiers::CONTROL) => self.chmod(screen)?,
            (KeyCode::F(2), _) => self.rename(screen)?,
            (KeyCode::F(3), _) => {
                if let Some(path) = self.front().current().filter(|entry| !entry.is_parent()).map(|entry| entry.path.clone()) {
                    self.viewer(screen, &path)?;
                }
            }
            (KeyCode::F(4), _) => self.edit(screen)?,
            (KeyCode::F(5), _) => self.transfer(screen, OpKind::Copy)?,
            (KeyCode::F(6), _) => self.transfer(screen, OpKind::Move)?,
            (KeyCode::F(7), m) if m.contains(KeyModifiers::ALT) => self.deep_find(screen)?,
            (KeyCode::F(7), _) => self.mkdir(screen)?,
            (KeyCode::F(8), _) => self.delete(screen)?,
            (KeyCode::F(9), _) => self.shell(screen)?,
            (KeyCode::F(10), _) => self.quit = true,
            (KeyCode::Char('q'), m) if m.contains(KeyModifiers::CONTROL) => self.quit = true,
            (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => self.goto(screen)?,
            (KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL) => self.connect_server(screen)?,
            (KeyCode::Char('h'), m) if m.contains(KeyModifiers::CONTROL) => {
                let hidden = !self.front().show_hidden;
                for panel in &mut self.panels { panel.show_hidden = hidden; }
                self.refresh();
            }
            (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => self.refresh(),
            (KeyCode::Char('/'), _) => self.find(screen)?,
            (KeyCode::Char('f'), m) if m.contains(KeyModifiers::CONTROL) => self.find(screen)?,
            _ => {}
        }
        Ok(())
    }

    fn help(&self, screen: &mut Screen) -> io::Result<()> {
        let lines = [
            "Dva panely: Tab · šipky · Enter otevře · Backspace nahoru",
            "Insert/mezerník: označit · PageUp/PageDown: posun",
            "F2 přejmenovat    Ctrl+F2 chmod    F3 text/HEX    F4 editor ($EDITOR)",
            "F5 kopírovat     F6 přesunout   F7 nový adresář",
            "F8 trvale smazat (po potvrzení) · F9 shell · F10 konec",
            "Ctrl+L cesta   Ctrl+H skryté   Ctrl+R obnovit",
            "Alt+F7: rekurzivní hledání jména a obsahu (lokální)",
            "Ctrl+K uložený / nový SFTP server · / najít jméno",
            "V archivu: Enter procházet · F5 rozbalit do druhého panelu",
            "Přenosy: lokální ↔ lokální a lokální ↔ SFTP",
            "Esc při kopírování přeruší operaci; data se nevymažou.",
        ].map(str::to_owned).to_vec();
        loop {
            screen.term.draw(|frame| {
                ui::draw(frame, self);
                ui::overlay(frame, "Nápověda Salamander RS TUI", &lines, "Esc / Enter / F1 zavřít");
            })?;
            if let Event::Key(key) = event::read()? {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::F(1)) { break; }
            }
        }
        Ok(())
    }

    fn goto(&mut self, screen: &mut Screen) -> io::Result<()> {
        let current = self.front().display_path();
        if let Some(text) = dialog_input(self, screen, "Přejít do adresáře", &current, false)? {
            let path = if let Some((id, remote)) = remote::resolve_typed(&text) {
                remote::virtual_path(&id, &remote)
            } else if let Some(home) = text.strip_prefix('~') {
                match std::env::var_os("HOME") {
                    Some(base) if home.is_empty() || home.starts_with('/') => PathBuf::from(base).join(home.trim_start_matches('/')),
                    _ => PathBuf::from(text),
                }
            } else {
                PathBuf::from(text)
            };
            if let Err(error) = self.front_mut().change_dir(path) { self.status = error; }
        }
        Ok(())
    }

    fn find(&mut self, screen: &mut Screen) -> io::Result<()> {
        if let Some(query) = dialog_input(self, screen, "Rychlé hledání v panelu", "", false)? {
            let query = query.to_lowercase();
            if let Some(i) = self.front().entries.iter().position(|entry| entry.folded_name.contains(query.as_str()) && !entry.is_parent()) {
                self.front_mut().cursor = i;
            } else { self.status = format!("„{query}“ nenalezeno"); }
        }
        Ok(())
    }

    fn deep_find(&mut self, screen: &mut Screen) -> io::Result<()> {
        if !self.front().is_local() {
            self.status = "Rekurzivní hledání zatím funguje jen na lokálním disku".into();
            return Ok(());
        }
        let Some(mask) = dialog_input(self, screen, "Hledání: maska názvu (např. *.rs)", "*", false)? else { return Ok(()); };
        let Some(content) = dialog_input(self, screen, "Text uvnitř souborů (prázdné = nehledat)", "", false)? else { return Ok(()); };
        let query = Query {
            root: self.front().path.clone(),
            mask: model::Mask::parse(&mask),
            content: content.into_bytes(),
            case_sensitive: false,
            include_hidden: self.front().show_hidden,
            include_dirs: true,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let (tx, rx) = mpsc::channel::<FindEvent>();
        let worker = thread::spawn(move || {
            let mut found = 0;
            find::search(&query, &thread_cancel, |event| {
                if matches!(&event, FindEvent::Found(_)) { found += 1; }
                tx.send(event).is_ok() && found < 2_000
            });
        });
        let mut results = Vec::<PathBuf>::new();
        let mut done = false;
        let mut was_cancelled = false;
        while !done {
            while let Ok(event) = rx.try_recv() {
                match event {
                    FindEvent::Directory(dir) => self.status = format!("Hledání: {}  (Esc přerušit)", dir.display()),
                    FindEvent::Found(path) => results.push(path),
                    FindEvent::Finished { found, errors, cancelled } => {
                        done = true;
                        was_cancelled = cancelled;
                        self.status = format!("Hledání: {found} výsledků, {errors} chyb{}", if cancelled { ", přerušeno nebo dosažen limit 2000" } else { "" });
                    }
                }
            }
            if !done {
                self.render(screen)?;
                if event::poll(Duration::from_millis(80))? {
                    if let Event::Key(key) = event::read()? {
                        if key.code == KeyCode::Esc { cancel.store(true, Ordering::Relaxed); }
                    }
                }
            }
        }
        let _ = worker.join();
        if results.is_empty() { return Ok(()); }
        if let Some(file) = select_result(self, screen, &results)? {
            if let Some(parent) = file.parent() {
                if let Err(error) = self.front_mut().change_dir(parent.to_path_buf()) {
                    self.status = error;
                } else if let Some(i) = self.front().entries.iter().position(|item| item.path == file) {
                    self.front_mut().cursor = i;
                }
            }
        }
        if was_cancelled { self.status.push_str(" [výsledky mohou být neúplné]"); }
        Ok(())
    }

    fn mkdir(&mut self, screen: &mut Screen) -> io::Result<()> {
        let Some(name) = dialog_input(self, screen, "Nový adresář", "", false)? else { return Ok(()); };
        if !valid_basename(&name) { self.status = "Neplatný název adresáře".into(); return Ok(()); }
        let base = self.front().path.clone();
        let outcome = if let Some((id, remote_dir)) = self.front().remote() {
            match remote::conn::get(&id) {
                Some(conn) => transfer::mkdir(&conn, &remote_dir.join(name)),
                None => Err("SFTP server není připojen".into()),
            }
        } else if self.front().archive().is_some() {
            Err("V archivu zatím nelze vytvářet adresáře".into())
        } else {
            fs::create_dir(base.join(name)).map_err(|error| error.to_string())
        };
        match outcome { Ok(()) => { self.status = "Adresář vytvořen".into(); self.refresh(); }, Err(error) => self.status = error }
        Ok(())
    }

    fn rename(&mut self, screen: &mut Screen) -> io::Result<()> {
        let Some(entry) = self.front().current() else { return Ok(()); };
        if entry.is_parent() { return Ok(()); }
        let path = entry.path.clone();
        let name = entry.name.to_string_lossy().into_owned();
        let Some(new_name) = dialog_input(self, screen, "Přejmenovat", &name, false)? else { return Ok(()); };
        if !valid_basename(&new_name) { self.status = "Neplatný název souboru".into(); return Ok(()); }
        let outcome = if let Some((id, remote_path)) = remote::split_virtual(&path) {
            match remote::conn::get(&id) {
                Some(conn) => {
                    let target = remote_path.with_file_name(&new_name);
                    // Prevent accidental overwrites even if the server permits rename-over-existing.
                    match conn.browse(|sftp| match sftp.lstat(&target) {
                        Ok(_) => Ok(true),
                        Err(e) if e.code() == Some(remote::sftp::STATUS_NO_SUCH_FILE) => Ok(false),
                        Err(e) => Err(e),
                    }) {
                        Ok(true) => Err("Cíl již existuje".into()),
                        Ok(false) => transfer::rename(&conn, &remote_path, &target),
                        Err(error) => Err(format!("Nelze ověřit cílový soubor: {error}")),
                    }
                }
                None => Err("SFTP není připojeno".into()),
            }
        } else if self.front().archive().is_some() {
            Err("Přejmenování uvnitř archivu není zatím podporováno".into())
        } else {
            let target = path.with_file_name(new_name);
            match fs::symlink_metadata(&target) {
                Ok(_) => Err("Cíl již existuje".into()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(path, target).map_err(|e| e.to_string()),
                Err(error) => Err(format!("Nelze ověřit cíl: {error}")),
            }
        };
        match outcome { Ok(()) => { self.status = "Přejmenováno".into(); self.refresh(); }, Err(error) => self.status = error }
        Ok(())
    }

    /// Change permission bits with the same core attribute engine as GTK. Symlinks are
    /// deliberately not followed by the local engine; SFTP symlinks are also skipped.
    fn chmod(&mut self, screen: &mut Screen) -> io::Result<()> {
        let files = self.front().selected_paths();
        if files.is_empty() { return Ok(()); }
        if self.front().archive().is_some() {
            self.status = "Práva uvnitř archivu nelze měnit".into();
            return Ok(());
        }
        let initial = self.front().current()
            .map(|entry| format!("{:04o}", entry.mode & 0o7777))
            .unwrap_or_else(|| "0644".to_owned());
        let Some(octal) = dialog_input(self, screen, "Změnit práva: osmičkově (např. 0750)", &initial, false)? else { return Ok(()); };
        let Some(mode) = attrs::parse_octal(&octal) else {
            self.status = "Neplatná osmičková práva (0000 až 7777)".into();
            return Ok(());
        };
        if let Some((id, _)) = self.front().remote() {
            let Some(conn) = remote::conn::get(&id) else { self.status = "SFTP není připojeno".into(); return Ok(()); };
            let mut done = 0;
            let mut errors = Vec::new();
            for path in files {
                let symlink = self.front().entries.iter().any(|entry| entry.path == path && entry.kind == EntryKind::Symlink);
                if symlink { errors.push(format!("{}: symbolický odkaz (přeskočeno)", path.display())); continue; }
                let Some((_, remote_path)) = remote::split_virtual(&path) else { continue; };
                match transfer::chmod(&conn, &remote_path, mode) {
                    Ok(()) => done += 1,
                    Err(error) => errors.push(format!("{}: {error}", path.display())),
                }
            }
            self.status = format!("SFTP chmod {:04o}: {} položek, {} chyb", mode, done, errors.len());
            if let Some(first) = errors.first() { self.status.push_str(&format!(" | {first}")); }
        } else {
            let change = AttrChange {
                mode: Some(ModeChange { set: mode, clear: 0o7777 & !mode, exec_only_dirs: false }),
                ..AttrChange::default()
            };
            let result = attrs::apply(&files, &change, &AtomicBool::new(false), |_, _| {});
            self.status = format!("chmod {:04o}: {} položek, {} chyb", mode, result.changed, result.errors.len());
            if let Some(first) = result.errors.first() {
                self.status.push_str(&format!(" | {}: {}", first.source.display(), first.error));
            }
        }
        self.refresh();
        Ok(())
    }

    fn delete(&mut self, screen: &mut Screen) -> io::Result<()> {
        let files = self.front().selected_paths();
        if files.is_empty() { return Ok(()); }
        if self.front().archive().is_some() {
            self.status = "Mazání uvnitř archivu není v TUI podporováno".into();
            return Ok(());
        }
        let warning = format!("TRVALE odstranit {} položek?\nTato operace nepoužívá koš.", files.len());
        if !dialog_confirm(self, screen, "Potvrzení mazání", &warning)? { return Ok(()); }
        if let Some((id, _)) = self.front().remote() {
            let Some(conn) = remote::conn::get(&id) else { self.status = "SFTP není připojeno".into(); return Ok(()); };
            let remote_files = files.iter().filter_map(|path| remote::split_virtual(path).map(|(_, path)| path)).collect::<Vec<_>>();
            let result = self.job(screen, "Mazání SFTP", move |cancel, tx| {
                transfer::delete(&conn, &remote_files, &cancel, &mut |path| { let _ = tx.send(path.to_path_buf()); })
            })?;
            self.report_job("Mazání SFTP", &result);
        } else {
            let errors = ops::delete_permanently(files);
            self.status = format!("Mazání: {} chyb", errors.len());
            if let Some(error) = errors.first() { self.status.push_str(&format!(" | {}: {}", error.source.display(), error.error)); }
        }
        self.front_mut().marked.clear();
        self.refresh();
        Ok(())
    }

    fn transfer(&mut self, screen: &mut Screen, kind: OpKind) -> io::Result<()> {
        let files = self.front().selected_paths();
        if files.is_empty() { return Ok(()); }
        let dest = self.other().path.clone();
        let source = self.front().path.clone();
        let action = if kind == OpKind::Copy { "Kopírování" } else { "Přesun" };
        if self.other().archive().is_some() {
            self.status = "Zápis do archivu z TUI zatím není podporován".into();
            return Ok(());
        }
        if let Some((archive_file, inner)) = archive::split_virtual(&source) {
            if kind == OpKind::Move || self.other().remote().is_some() {
                self.status = "Z archivu lze zatím pouze rozbalovat do lokálního adresáře".into();
                return Ok(());
            }
            let items = files.iter().filter_map(|item| archive::split_virtual(item).map(|(_, inner)| inner)).collect::<Vec<_>>();
            let overwrite = dialog_confirm(self, screen, "Rozbalit archiv", "Povolit přepsání existujících souborů?")?;
            let result = self.job(screen, "Rozbalování", move |cancel, tx| {
                archive::extract(&ExtractRequest { archive: &archive_file, items: &items, base: &inner, dest: &dest, overwrite }, &cancel,
                    &mut |path| { let _ = tx.send(path.to_path_buf()); })
            })?;
            self.report_job("Rozbalování", &result);
        } else if let Some((id, _)) = self.front().remote() {
            if kind == OpKind::Move || self.other().remote().is_some() {
                self.status = "Přesun SFTP a SFTP→SFTP zatím není podporován (použij F5)".into();
                return Ok(());
            }
            let Some(conn) = remote::conn::get(&id) else { self.status = "SFTP není připojeno".into(); return Ok(()); };
            let remote_paths = files.iter().filter_map(|path| remote::split_virtual(path).map(|(_, remote)| remote)).collect::<Vec<_>>();
            let overwrite = dialog_confirm(self, screen, "Stahování SFTP", "Povolit přepsání existujících souborů?")?;
            let result = self.job(screen, "Stahování", move |cancel, tx| {
                transfer::download(&conn, &remote_paths, &dest, overwrite, &cancel, &mut |path| { let _ = tx.send(path.to_path_buf()); })
            })?;
            self.report_job("Stahování", &result);
        } else if let Some((id, remote_dir)) = self.other().remote() {
            if kind == OpKind::Move {
                self.status = "Přesun lokální→SFTP není podporován; použij F5 (kopírování)".into();
                return Ok(());
            }
            let Some(conn) = remote::conn::get(&id) else { self.status = "SFTP není připojeno".into(); return Ok(()); };
            let overwrite = dialog_confirm(self, screen, "Odesílání SFTP", "Povolit přepsání existujících souborů?")?;
            let result = self.job(screen, "Nahrávání", move |cancel, tx| {
                transfer::upload(&conn, &files, &remote_dir, overwrite, &cancel, &mut |path| { let _ = tx.send(path.to_path_buf()); })
            })?;
            self.report_job("Nahrávání", &result);
        } else {
            self.local_transfer(screen, Op { kind, sources: files, dest_dir: dest })?;
        }
        self.front_mut().marked.clear();
        self.refresh();
        self.status = format!("{action}: {}", self.status);
        Ok(())
    }

    fn local_transfer(&mut self, screen: &mut Screen, op: Op) -> io::Result<()> {
        let (tx, rx) = async_channel::unbounded::<OpEvent>();
        let (reply_tx, reply_rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = ops::start(op, tx, reply_rx, Arc::clone(&cancel));
        let mut finished = false;
        while !finished {
            loop {
                match rx.try_recv() {
                    Ok(OpEvent::Preparing { current }) => self.status = format!("Příprava: {} [Esc přerušit]", current.display()),
                    Ok(OpEvent::Progress(p)) => {
                        self.status = format!("Kopírování: {}/{} souborů · {} / {} · {} [Esc přerušit]",
                            p.done_files, p.total_files, model::format_bytes(p.done_bytes),
                            model::format_bytes(p.total_bytes), p.current.display());
                    }
                    Ok(OpEvent::Conflict(conflict)) => {
                        let message = format!("Cíl již existuje:\n{}\nZdroj:\n{}", conflict.target.display(), conflict.source.display());
                        let answer = dialog_choice(self, screen, "Konflikt při kopírování", &message,
                            "P přepsat / S přeskočit / A vše / N nic / Esc zrušit")?;
                        let action = match answer {
                            Some('p') => ConflictAction::Overwrite,
                            Some('s') => ConflictAction::Skip,
                            Some('a') => ConflictAction::OverwriteAll,
                            Some('n') => ConflictAction::SkipAll,
                            _ => { cancel.store(true, Ordering::Relaxed); ConflictAction::Abort },
                        };
                        let _ = reply_tx.send(action);
                    }
                    Ok(OpEvent::Finished(result)) => {
                        self.status = format!("Hotovo: {} chyb, stav {:?}", result.errors.len(), result.outcome);
                        if let Some(error) = result.errors.first() {
                            self.status.push_str(&format!(" | {}: {}", error.source.display(), error.error));
                        }
                        finished = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => { finished = true; break; }
                }
            }
            if !finished {
                self.render(screen)?;
                if event::poll(Duration::from_millis(60))? {
                    if let Event::Key(key) = event::read()? {
                        if key.code == KeyCode::Esc { cancel.store(true, Ordering::Relaxed); }
                    }
                }
            }
        }
        let _ = worker.join();
        Ok(())
    }

    /// Generic blocking worker with live progress; no terminal access from worker thread.
    fn job<F>(&mut self, screen: &mut Screen, name: &str, task: F) -> io::Result<JobResult>
    where F: FnOnce(Arc<AtomicBool>, mpsc::Sender<PathBuf>) -> JobResult + Send + 'static {
        let (progress_tx, progress_rx) = mpsc::channel::<PathBuf>();
        let (result_tx, result_rx) = mpsc::channel::<JobResult>();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker = thread::spawn(move || { let _ = result_tx.send(task(worker_cancel, progress_tx)); });
        let result = loop {
            match result_rx.try_recv() {
                Ok(result) => break result,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let mut error = JobResult::default();
                    error.errors.push(model::OpError {
                        source: PathBuf::from("<background worker>"),
                        target: None,
                        error: "Pracovní vlákno skončilo bez výsledku".into(),
                    });
                    break error;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            while let Ok(path) = progress_rx.try_recv() {
                self.status = format!("{name}: {}  [Esc přerušit]", path.display());
            }
            self.render(screen)?;
            if event::poll(Duration::from_millis(80))? {
                if let Event::Key(key) = event::read()? {
                    if key.code == KeyCode::Esc { cancel.store(true, Ordering::Relaxed); }
                }
            }
        };
        let _ = worker.join();
        Ok(result)
    }

    fn edit(&mut self, screen: &mut Screen) -> io::Result<()> {
        let Some(file) = self.front().current().filter(|entry| entry.kind == EntryKind::File).map(|entry| entry.path.clone()) else { return Ok(()); };
        if !self.front().is_local() {
            self.status = "F4 upravuje pouze lokální soubory; SFTP a archiv jsou jen pro čtení".into();
            return Ok(());
        }
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
        let mut words = editor.split_whitespace();
        let Some(exe) = words.next() else { self.status = "Neplatná proměnná $EDITOR".into(); return Ok(()); };
        let mut command = Command::new(exe);
        command.args(words).arg(file);
        if let Err(error) = screen.suspend(&mut command) { self.status = format!("Editor: {error}"); }
        self.refresh();
        Ok(())
    }

    fn shell(&mut self, screen: &mut Screen) -> io::Result<()> {
        let dir = self.front().path.clone();
        let mut command = if let Some((id, remote_dir)) = self.front().remote() {
            let Some(conn) = remote::conn::get(&id) else { self.status = "SFTP není připojeno".into(); return Ok(()); };
            let args = conn.terminal_command(&remote_dir);
            let mut cmd = Command::new(&args[0]);
            cmd.args(&args[1..]);
            cmd
        } else if self.front().archive().is_some() {
            self.status = "Shell nelze spustit uvnitř archivu".into();
            return Ok(());
        } else {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let mut cmd = Command::new(shell);
            cmd.current_dir(dir);
            cmd
        };
        if let Err(error) = screen.suspend(&mut command) { self.status = error.to_string(); }
        self.refresh();
        Ok(())
    }

    fn viewer(&mut self, screen: &mut Screen, path: &Path) -> io::Result<()> {
        let file = if let Some((archive, inner)) = archive::split_virtual(path) {
            match archive::extract_to_temp(&archive, &inner) {
                Ok(file) => file,
                Err(error) => { self.status = error; return Ok(()); }
            }
        } else if let Some((id, remote)) = remote::split_virtual(path) {
            match remote::conn::get(&id) {
                Some(conn) => match transfer::download_to_temp(&conn, &remote) {
                    Ok(file) => file,
                    Err(error) => { self.status = error; return Ok(()); }
                },
                None => { self.status = "SFTP není připojeno".into(); return Ok(()); }
            }
        } else { path.to_path_buf() };
        let mut data = Vec::new();
        match fs::File::open(&file).and_then(|f| f.take(512 * 1024).read_to_end(&mut data)) {
            Ok(_) => {},
            Err(error) => { self.status = format!("Prohlížeč: {error}"); return Ok(()); }
        }
        let name = path.to_string_lossy().into_owned();
        let mut hex = false;
        let mut scroll = 0usize;
        loop {
            let content = if hex { hex_lines(&data) } else { text_lines(&data) };
            screen.term.draw(|frame| {
                ui::draw(frame, self);
                let area = frame.area();
                let rect = ui::centered(area.width.saturating_sub(2), area.height.saturating_sub(2), area);
                frame.render_widget(Clear, rect);
                let block = Block::default().borders(Borders::ALL).title(format!(" F3 {} {} ", if hex { "HEX" } else { "TEXT" }, name));
                let inside = block.inner(rect);
                frame.render_widget(block, rect);
                let count = usize::from(inside.height.saturating_sub(1));
                let text = content.iter().skip(scroll).take(count).cloned().collect::<Vec<_>>().join("\n");
                frame.render_widget(Paragraph::new(text), Rect::new(inside.x, inside.y, inside.width, inside.height.saturating_sub(1)));
                frame.render_widget(Paragraph::new("H text/hex · ↑↓ posun · PgUp/PgDn · Esc zavřít")
                    .style(Style::default().fg(Color::Yellow)),
                    Rect::new(inside.x, inside.y + inside.height.saturating_sub(1), inside.width, 1));
            })?;
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Esc | KeyCode::F(3) => break,
                    KeyCode::Char('h') | KeyCode::Char('H') => { hex = !hex; scroll = 0; },
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::Down => scroll = (scroll + 1).min(content.len().saturating_sub(1)),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(20),
                    KeyCode::PageDown => scroll = (scroll + 20).min(content.len().saturating_sub(1)),
                    KeyCode::Home => scroll = 0,
                    KeyCode::End => scroll = content.len().saturating_sub(1),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Create an SFTP profile entirely inside the terminal, without needing the GTK app.
    fn create_server(&mut self, screen: &mut Screen) -> io::Result<Option<servers::Server>> {
        let mut server = servers::Server::new();
        let Some(host) = dialog_input(self, screen, "Nový SFTP server: adresa nebo IP", "", false)? else { return Ok(None); };
        server.host = host.trim().to_owned();
        let Some(user) = dialog_input(self, screen, "SFTP: uživatelské jméno", &server.user, false)? else { return Ok(None); };
        server.user = user.trim().to_owned();
        let Some(port) = dialog_input(self, screen, "SFTP: SSH port", "22", false)? else { return Ok(None); };
        match port.trim().parse::<u16>() {
            Ok(port) if port != 0 => server.port = port,
            _ => { self.status = "Neplatný TCP port (1–65535)".into(); return Ok(None); }
        }
        let Some(name) = dialog_input(self, screen, "SFTP: název profilu (volitelné)", &server.label(), false)? else { return Ok(None); };
        server.name = name;
        let Some(initial_dir) = dialog_input(self, screen, "SFTP: počáteční adresář (prázdné = HOME)", "", false)? else { return Ok(None); };
        server.initial_dir = initial_dir;
        let Some(identity) = dialog_input(self, screen, "SFTP: cesta k privátnímu SSH klíči (volitelné)", "", false)? else { return Ok(None); };
        if !identity.trim().is_empty() {
            server.identity = Some(PathBuf::from(identity.trim()));
            server.auth = servers::AuthMethod::Key;
        }
        let Some(options) = dialog_input(self, screen, "SFTP: další SSH přepínače (volitelné)", "", false)? else { return Ok(None); };
        server.options = options;
        if let Err(error) = server.validate() {
            self.status = format!("Neplatný SFTP profil: {error}");
            return Ok(None);
        }
        let path = servers::default_path();
        let mut all = servers::load(&path);
        all.push(server.clone());
        if let Err(error) = servers::save(&path, &all) {
            self.status = error;
            return Ok(None);
        }
        self.status = format!("SFTP profil {} uložen (bez hesla)", server.label());
        Ok(Some(server))
    }

    fn connect_server(&mut self, screen: &mut Screen) -> io::Result<()> {
        let all = servers::load(&servers::default_path());
        let server = if all.is_empty() {
            self.create_server(screen)?
        } else {
            match select_server(self, screen, &all)? {
                Some(ServerAction::Existing(server)) => Some(server),
                Some(ServerAction::New) => self.create_server(screen)?,
                None => None,
            }
        };
        let Some(server) = server else { return Ok(()); };
        let selected = server.id.clone();
        if let Some(conn) = remote::conn::get(&selected) {
            let directory = if server.initial_dir.starts_with('/') { PathBuf::from(&server.initial_dir) } else { conn.home.clone() };
            if let Err(error) = self.front_mut().change_dir(remote::virtual_path(&selected, &directory)) { self.status = error; }
            return Ok(());
        }
        struct Request {
            kind: PromptKind,
            text: String,
            reply: mpsc::SyncSender<Option<Secret>>,
        }
        let (prompt_tx, prompt_rx) = mpsc::channel::<Request>();
        let (result_tx, result_rx) = mpsc::channel();
        thread::spawn(move || {
            let handler: remote::conn::PromptHandler = Box::new(move |kind, text| {
                let (tx, rx) = mpsc::sync_channel(1);
                prompt_tx.send(Request { kind, text: text.into(), reply: tx }).ok()?;
                rx.recv().ok().flatten()
            });
            let _ = result_tx.send(Connection::connect(server, handler));
        });
        self.status = "Připojuji se přes OpenSSH …".into();
        loop {
            while let Ok(request) = prompt_rx.try_recv() {
                let answer = match request.kind {
                    PromptKind::HostKey => {
                        if dialog_confirm(self, screen, "Ověření otisku SSH serveru", &request.text)? { Some(Secret::from_str("yes")) } else { None }
                    }
                    PromptKind::Password | PromptKind::Passphrase | PromptKind::Other => {
                        dialog_secret(self, screen, "SSH ověření", &request.text)?
                    }
                };
                let _ = request.reply.send(answer);
            }
            match result_rx.try_recv() {
                Ok(Ok(conn)) => {
                    let directory = if conn.server.initial_dir.starts_with('/') { PathBuf::from(&conn.server.initial_dir) } else { conn.home.clone() };
                    if let Err(error) = self.front_mut().change_dir(remote::virtual_path(&selected, &directory)) { self.status = error; }
                    else { self.status = format!("SFTP připojeno: {}", conn.label()); }
                    break;
                }
                Ok(Err(error)) => { self.status = format!("SSH: {error}"); break; }
                Err(mpsc::TryRecvError::Disconnected) => { self.status = "Připojení SSH skončilo neočekávaně".into(); break; }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            self.render(screen)?;
            // During connection the worker owns ssh; prompts are answered here.
            // No unrelated keys are forwarded to other UI actions.
            if event::poll(Duration::from_millis(80))? { let _ = event::read()?; }
        }
        Ok(())
    }

    fn save_config(&self) {
        let mut config = Config::load();
        if self.panels[0].is_local() { config.left_dir = Some(self.panels[0].path.clone()); }
        if self.panels[1].is_local() { config.right_dir = Some(self.panels[1].path.clone()); }
        config.show_hidden = self.panels[0].show_hidden;
        let _ = config.save();
    }
}

fn valid_basename(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

fn dialog_input(app: &App, screen: &mut Screen, title: &str, initial: &str, hide: bool) -> io::Result<Option<String>> {
    let mut input = initial.to_owned();
    loop {
        let view = if hide { "*".repeat(input.chars().count()) } else { input.clone() };
        screen.term.draw(|frame| {
            ui::draw(frame, app);
            ui::overlay(frame, title, &[format!("> {view}")], "Enter potvrdit · Esc zrušit · Ctrl+U vymazat");
        })?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press { continue; }
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => return Ok(None),
                (KeyCode::Enter, _) => return Ok(Some(input)),
                (KeyCode::Backspace, _) => { input.pop(); }
                (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => input.clear(),
                (KeyCode::Char(ch), modifiers) if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && !ch.is_control() => {
                    if input.len() < 4096 { input.push(ch); }
                }
                _ => {}
            }
        }
    }
}

/// Bytes are never echoed or stored in the config; the sodium::Secret wipes them on drop.
fn dialog_secret(app: &App, screen: &mut Screen, title: &str, prompt: &str) -> io::Result<Option<Secret>> {
    let mut bytes = Vec::<u8>::new();
    loop {
        screen.term.draw(|frame| {
            ui::draw(frame, app);
            ui::overlay(frame, title, &[prompt.to_owned(), format!("> {}", "*".repeat(bytes.len().min(100)))], "Enter potvrdit · Esc odmítnout");
        })?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press { continue; }
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => {
                    drop(Secret::new(std::mem::take(&mut bytes)));
                    return Ok(None);
                },
                (KeyCode::Enter, _) => return Ok(Some(Secret::new(std::mem::take(&mut bytes)))),
                (KeyCode::Backspace, _) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        if let Some(ch) = text.chars().last() {
                            let length = ch.len_utf8();
                            let start = bytes.len() - length;
                            for byte in &mut bytes[start..] { *byte = 0; }
                            bytes.truncate(start);
                        }
                    }
                }
                (KeyCode::Char(ch), modifiers) if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && !ch.is_control() => {
                    if bytes.len() < 4096 {
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
                _ => {}
            }
        }
    }
}

fn dialog_choice(app: &App, screen: &mut Screen, title: &str, message: &str, hint: &str) -> io::Result<Option<char>> {
    loop {
        screen.term.draw(|frame| {
            ui::draw(frame, app);
            ui::overlay(frame, title, &message.lines().map(str::to_owned).collect::<Vec<_>>(), hint);
        })?;
        if let Event::Key(key) = event::read()? {
            if key.code == KeyCode::Esc { return Ok(None); }
            if let KeyCode::Char(ch) = key.code {
                if "psan".contains(ch.to_ascii_lowercase()) { return Ok(Some(ch.to_ascii_lowercase())); }
            }
        }
    }
}

fn dialog_confirm(app: &App, screen: &mut Screen, title: &str, message: &str) -> io::Result<bool> {
    loop {
        screen.term.draw(|frame| {
            ui::draw(frame, app);
            ui::overlay(frame, title, &message.lines().map(str::to_owned).collect::<Vec<_>>(), "A ano · N / Enter / Esc ne");
        })?;
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => return Ok(false),
                _ => {}
            }
        }
    }
}

fn select_result(app: &App, screen: &mut Screen, list: &[PathBuf]) -> io::Result<Option<PathBuf>> {
    let mut index = 0usize;
    loop {
        let start = index.saturating_sub(7).min(list.len().saturating_sub(15));
        let mut lines = vec![format!("Výsledek {} z {}", index + 1, list.len())];
        lines.extend(list.iter().enumerate().skip(start).take(15).map(|(i, path)| {
            format!("{} {}", if i == index { '>' } else { ' ' }, path.display())
        }));
        screen.term.draw(|frame| {
            ui::draw(frame, app);
            ui::overlay(frame, "Výsledky rekurzivního hledání", &lines, "↑↓ vybrat · Enter přejít · Esc zavřít");
        })?;
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter => return Ok(Some(list[index].clone())),
                KeyCode::Up => index = index.saturating_sub(1),
                KeyCode::Down => index = (index + 1).min(list.len() - 1),
                KeyCode::PageUp => index = index.saturating_sub(15),
                KeyCode::PageDown => index = (index + 15).min(list.len() - 1),
                _ => {}
            }
        }
    }
}

enum ServerAction {
    Existing(servers::Server),
    New,
}

fn select_server(app: &App, screen: &mut Screen, list: &[servers::Server]) -> io::Result<Option<ServerAction>> {
    let mut index = 0usize;
    loop {
        let start = index.saturating_sub(7).min(list.len().saturating_sub(15));
        let mut labels = vec![format!("Profil {} z {}", index + 1, list.len())];
        labels.extend(list.iter().enumerate().skip(start).take(15).map(|(i, s)| {
            format!("{} {:<22} {}", if i == index { '>' } else { ' ' }, s.display_name(), s.label())
        }));
        screen.term.draw(|frame| {
            ui::draw(frame, app);
            ui::overlay(frame, "Uložené SFTP servery", &labels, "↑↓ vybrat · Enter připojit · N nový server · Esc zrušit");
        })?;
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Char('n') | KeyCode::Char('N') => return Ok(Some(ServerAction::New)),
                KeyCode::Enter => return Ok(Some(ServerAction::Existing(list[index].clone()))),
                KeyCode::Up => index = index.saturating_sub(1),
                KeyCode::Down => index = (index + 1).min(list.len() - 1),
                KeyCode::PageUp => index = index.saturating_sub(15),
                KeyCode::PageDown => index = (index + 15).min(list.len() - 1),
                _ => {}
            }
        }
    }
}

fn text_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes).lines().map(|line| {
        line.chars().map(|ch| if ch.is_control() && ch != '\t' { ' ' } else { ch }).collect()
    }).take(15000).collect()
}

fn hex_lines(bytes: &[u8]) -> Vec<String> {
    bytes.chunks(16).enumerate().map(|(i, chunk)| {
        let mut hex = chunk.iter().map(|b| format!("{b:02x} ")).collect::<String>();
        while hex.len() < 48 { hex.push(' '); }
        let printable = chunk.iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect::<String>();
        format!("{:08x}  {} {}", i * 16, hex, printable)
    }).collect()
}

fn launch_args() -> Result<(PathBuf, PathBuf, bool), String> {
    let cfg = Config::load();
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let args = std::env::args_os().skip(1).filter(|arg| arg.to_str() != Some("--tui")).collect::<Vec<OsString>>();
    if args.iter().any(|arg| matches!(arg.to_str(), Some("--help") | Some("-h"))) {
        println!("Salamander RS TUI — Rust terminálový správce souborů\nPoužití: salamander-rs-tui [LEVÝ_ADRESÁŘ] [PRAVÝ_ADRESÁŘ]\nOvládání: F1 nápověda, F5 kopírovat, F6 přesunout, F10 konec.\nSpuštění vyžaduje interaktivní terminál (TTY).");
        std::process::exit(0);
    }
    if args.len() > 2 { return Err("Příliš mnoho parametrů. Použij --help".into()); }
    let left = args.first().map(PathBuf::from).or(cfg.left_dir).unwrap_or_else(|| cwd.clone());
    let right = args.get(1).map(PathBuf::from).or(cfg.right_dir).unwrap_or(cwd);
    Ok((left, right, cfg.show_hidden))
}

fn main() {
    // OpenSSH starts this same compiled program as a private SSH_ASKPASS subprocess.
    // It must exit before any terminal initialization or argument parsing.
    if let Some(code) = remote::askpass::askpass_main() { std::process::exit(code); }
    if let Err(error) = run() {
        eprintln!("Salamander RS TUI: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (left, right, hidden) = launch_args()?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("TUI vyžaduje terminál (např. SSH -t, /dev/tty nebo tmux)".into());
    }
    archive::cleanup_temp();
    remote::askpass::cleanup_stale();
    let mut app = App::new(left, right, hidden)?;
    let mut screen = Screen::new()?;
    while !app.quit {
        app.render(&mut screen)?;
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => app.handle(key, &mut screen)?,
                Event::Mouse(mouse) => {
                    let height = screen.term.size()?.height;
                    if matches!(mouse.kind, MouseEventKind::ScrollDown | MouseEventKind::ScrollUp) {
                        let active = if mouse.column < screen.term.size()?.width / 2 { 0 } else { 1 };
                        app.active = active;
                        app.front_mut().move_cursor(if mouse.kind == MouseEventKind::ScrollDown { 3 } else { -3 }, ui::visible_rows(height));
                    }
                }
                _ => {}
            }
        }
    }
    app.save_config();
    drop(screen);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filenames_cannot_escape_parent() {
        assert!(valid_basename("Soubor 1.txt"));
        assert!(!valid_basename("../etc/passwd"));
        assert!(!valid_basename("."));
        assert!(!valid_basename(""));
        assert!(!valid_basename("a/b"));
    }
    #[test]
    fn hex_dump_has_offsets() {
        assert!(hex_lines(b"abc")[0].starts_with("00000000"));
        assert!(hex_lines(&[0u8; 20])[1].starts_with("00000010"));
    }
}
