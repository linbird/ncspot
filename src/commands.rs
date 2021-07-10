use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::command::{
    parse, Command, GotoMode, JumpMode, MoveAmount, MoveMode, SeekDirection, ShiftMode, TargetMode,
};
use crate::config::Config;
use crate::events::EventManager;
use crate::library::Library;
use crate::queue::{Queue, RepeatSetting};
use crate::spotify::{Spotify, VOLUME_PERCENT};
use crate::traits::{IntoBoxedViewExt, ViewExt};
use crate::ui::contextmenu::{AddToPlaylistMenu, ContextMenu, SelectArtistMenu};
use crate::ui::help::HelpView;
use crate::ui::layout::Layout;
use crate::ui::modal::Modal;
use crate::ui::search_results::SearchResultsView;
use crate::UserData;
use cursive::event::{Event, Key};
use cursive::traits::View;
use cursive::views::Dialog;
use cursive::Cursive;
use log::{debug, error, info};
use std::cell::RefCell;

pub enum CommandResult {
    Consumed(Option<String>),
    View(Box<dyn ViewExt>),
    Modal(Box<dyn View>),
    Ignored,
}

pub struct CommandManager {
    aliases: HashMap<String, String>,
    bindings: RefCell<HashMap<String, Command>>,
    spotify: Spotify,
    queue: Arc<Queue>,
    library: Arc<Library>,
    config: Arc<Config>,
    events: EventManager,
}

impl CommandManager {
    pub fn new(
        spotify: Spotify,
        queue: Arc<Queue>,
        library: Arc<Library>,
        config: Arc<Config>,
        events: EventManager,
    ) -> CommandManager {
        let bindings = RefCell::new(Self::get_bindings(config.clone()));
        CommandManager {
            aliases: HashMap::new(),
            bindings,
            spotify,
            queue,
            library,
            config,
            events,
        }
    }

    pub fn get_bindings(config: Arc<Config>) -> HashMap<String, Command> {
        let config = config.values();
        let mut kb = if config.default_keybindings.unwrap_or(true) {
            Self::default_keybindings()
        } else {
            HashMap::new()
        };
        let custom_bindings: Option<HashMap<String, String>> = config.keybindings.clone();

        for (key, command) in custom_bindings.unwrap_or_default() {
            if let Some(command) = parse(&command) {
                info!("Custom keybinding: {} -> {:?}", key, command);
                kb.insert(key, command);
            } else {
                error!("Invalid command for key {}: {}", key, command);
            }
        }

        kb
    }

    pub fn register_aliases<S: Into<String>>(&mut self, name: S, aliases: Vec<S>) {
        let name = name.into();
        for a in aliases {
            self.aliases.insert(a.into(), name.clone());
        }
    }

    pub fn register_all(&mut self) {
        self.register_aliases("quit", vec!["q", "x"]);
        self.register_aliases("playpause", vec!["pause", "toggleplay", "toggleplayback"]);
        self.register_aliases("repeat", vec!["loop"]);
    }

    fn handle_default_commands(
        &self,
        s: &mut Cursive,
        cmd: &Command,
    ) -> Result<Option<String>, String> {
        match cmd {
            Command::Noop => Ok(None),
            Command::Quit => {
                let queue = self.queue.queue.read().expect("can't readlock queue");
                self.config.with_state_mut(move |mut s| {
                    debug!(
                        "saving state, {} items, current track: {:?}",
                        queue.len(),
                        self.queue.get_current_index()
                    );
                    s.queuestate.queue = queue.clone();
                    s.queuestate.random_order = self.queue.get_random_order();
                    s.queuestate.current_track = self.queue.get_current_index();
                    s.queuestate.track_progress = self.spotify.get_current_progress();
                });
                self.config.save_state();
                s.quit();
                Ok(None)
            }
            Command::Stop => {
                self.queue.stop();
                Ok(None)
            }
            Command::Previous => {
                if self.spotify.get_current_progress() < Duration::from_secs(5) {
                    self.queue.previous();
                } else {
                    self.spotify.seek(0);
                }
                Ok(None)
            }
            Command::Next => {
                self.queue.next(true);
                Ok(None)
            }
            Command::Clear => {
                let queue = self.queue.clone();
                let confirmation = Dialog::text("Clear queue?")
                    .button("Yes", move |s| {
                        s.pop_layer();
                        queue.clear()
                    })
                    .dismiss_button("No");
                s.add_layer(Modal::new(confirmation));
                Ok(None)
            }
            Command::UpdateLibrary => {
                self.library.update_library();
                Ok(None)
            }
            Command::TogglePlay => {
                self.queue.toggleplayback();
                Ok(None)
            }
            Command::Shuffle(mode) => {
                let mode = mode.unwrap_or_else(|| !self.queue.get_shuffle());
                self.queue.set_shuffle(mode);
                Ok(None)
            }
            Command::Repeat(mode) => {
                let mode = mode.unwrap_or_else(|| match self.queue.get_repeat() {
                    RepeatSetting::None => RepeatSetting::RepeatPlaylist,
                    RepeatSetting::RepeatPlaylist => RepeatSetting::RepeatTrack,
                    RepeatSetting::RepeatTrack => RepeatSetting::None,
                });

                self.queue.set_repeat(mode);
                Ok(None)
            }
            Command::Seek(direction) => {
                match *direction {
                    SeekDirection::Relative(rel) => self.spotify.seek_relative(rel),
                    SeekDirection::Absolute(abs) => self.spotify.seek(abs),
                }
                Ok(None)
            }
            Command::VolumeUp(amount) => {
                let volume = self
                    .spotify
                    .volume()
                    .saturating_add(VOLUME_PERCENT * amount);
                self.spotify.set_volume(volume);
                Ok(None)
            }
            Command::VolumeDown(amount) => {
                let volume = self
                    .spotify
                    .volume()
                    .saturating_sub(VOLUME_PERCENT * amount);
                debug!("vol {}", volume);
                self.spotify.set_volume(volume);
                Ok(None)
            }
            Command::Help => {
                let view = Box::new(HelpView::new(self.bindings.borrow().clone()));
                s.call_on_name("main", move |v: &mut Layout| v.push_view(view));
                Ok(None)
            }
            Command::ReloadConfig => {
                self.config.reload();

                // update theme
                let theme = self.config.build_theme();
                s.set_theme(theme);

                // update bindings
                self.unregister_keybindings(s);
                self.bindings
                    .replace(Self::get_bindings(self.config.clone()));
                self.register_keybindings(s);
                Ok(None)
            }
            Command::NewPlaylist(name) => {
                match self.spotify.create_playlist(name, None, None) {
                    Some(_) => self.library.update_library(),
                    None => error!("could not create playlist {}", name),
                }
                Ok(None)
            }
            Command::Search(term) => {
                let view = if !term.is_empty() {
                    Some(SearchResultsView::new(
                        term.clone(),
                        self.events.clone(),
                        self.queue.clone(),
                        self.library.clone(),
                    ))
                } else {
                    None
                };
                s.call_on_name("main", |v: &mut Layout| {
                    v.set_screen("search");
                    if let Some(results) = view {
                        v.push_view(results.into_boxed_view_ext())
                    }
                });
                Ok(None)
            }
            Command::Logout => {
                self.spotify.shutdown();

                let mut credentials_path = crate::config::cache_path("librespot");
                credentials_path.push("credentials.json");
                std::fs::remove_file(credentials_path).unwrap();

                s.quit();
                Ok(None)
            }
            Command::Jump(_)
            | Command::Move(_, _)
            | Command::Shift(_, _)
            | Command::Play
            | Command::PlayNext
            | Command::Queue
            | Command::Save
            | Command::Delete
            | Command::Back
            | Command::Open(_)
            | Command::Insert(_)
            | Command::Goto(_) => Ok(None),
            _ => Err("Unknown Command".into()),
        }
    }

    fn handle_callbacks(&self, s: &mut Cursive, cmd: &Command) -> Result<Option<String>, String> {
        let local = if let Some(mut contextmenu) = s.find_name::<ContextMenu>("contextmenu") {
            contextmenu.on_command(s, cmd)?
        } else if let Some(mut add_track_menu) = s.find_name::<AddToPlaylistMenu>("addtrackmenu") {
            add_track_menu.on_command(s, cmd)?
        } else if let Some(mut select_artist) = s.find_name::<SelectArtistMenu>("selectartist") {
            select_artist.on_command(s, cmd)?
        } else {
            let mut main = s
                .find_name::<Layout>("main")
                .expect("could not find layout");
            main.on_command(s, cmd)?
        };

        if let CommandResult::Consumed(output) = local {
            Ok(output)
        } else if let CommandResult::Modal(modal) = local {
            s.add_layer(modal);
            Ok(None)
        } else if let CommandResult::View(view) = local {
            s.call_on_name("main", move |v: &mut Layout| {
                v.push_view(view);
            });

            Ok(None)
        } else {
            self.handle_default_commands(s, cmd)
        }
    }

    pub fn handle(&self, s: &mut Cursive, cmd: Command) {
        let result = self.handle_callbacks(s, &cmd);

        s.call_on_name("main", |v: &mut Layout| {
            v.set_result(result);
        });

        s.on_event(Event::Refresh);
    }

    pub fn register_keybinding<E: Into<cursive::event::Event>>(
        &self,
        cursive: &mut Cursive,
        event: E,
        command: Command,
    ) {
        cursive.add_global_callback(event, move |s| {
            if let Some(data) = s.user_data::<UserData>().cloned() {
                data.cmd.handle(s, command.clone());
            }
        });
    }

    pub fn unregister_keybindings(&self, cursive: &mut Cursive) {
        let kb = self.bindings.borrow();

        for (k, _v) in kb.iter() {
            if let Some(binding) = Self::parse_keybinding(&k) {
                cursive.clear_global_callbacks(binding);
            }
        }
    }

    pub fn register_keybindings(&self, cursive: &mut Cursive) {
        let kb = self.bindings.borrow();

        for (k, v) in kb.iter() {
            if let Some(binding) = Self::parse_keybinding(&k) {
                self.register_keybinding(cursive, binding, v.clone());
            } else {
                error!("Could not parse keybinding: \"{}\"", &k);
            }
        }
    }

    fn default_keybindings() -> HashMap<String, Command> {
        let mut kb = HashMap::new();

        kb.insert("q".into(), Command::Quit);
        kb.insert("Shift+p".into(), Command::TogglePlay);
        kb.insert("Shift+u".into(), Command::UpdateLibrary);
        kb.insert("Shift+s".into(), Command::Stop);
        kb.insert("<".into(), Command::Previous);
        kb.insert(">".into(), Command::Next);
        kb.insert("c".into(), Command::Clear);
        kb.insert("Space".into(), Command::Queue);
        kb.insert(".".into(), Command::PlayNext);
        kb.insert("Enter".into(), Command::Play);
        kb.insert("n".into(), Command::Jump(JumpMode::Next));
        kb.insert("Shift+n".into(), Command::Jump(JumpMode::Previous));
        kb.insert("s".into(), Command::Save);
        kb.insert("Ctrl+s".into(), Command::SaveQueue);
        kb.insert("d".into(), Command::Delete);
        kb.insert("f".into(), Command::Seek(SeekDirection::Relative(1000)));
        kb.insert("b".into(), Command::Seek(SeekDirection::Relative(-1000)));
        kb.insert(
            "Shift+f".into(),
            Command::Seek(SeekDirection::Relative(10000)),
        );
        kb.insert(
            "Shift+b".into(),
            Command::Seek(SeekDirection::Relative(-10000)),
        );
        kb.insert("+".into(), Command::VolumeUp(1));
        kb.insert("]".into(), Command::VolumeUp(5));
        kb.insert("-".into(), Command::VolumeDown(1));
        kb.insert("[".into(), Command::VolumeDown(5));

        kb.insert("r".into(), Command::Repeat(None));
        kb.insert("z".into(), Command::Shuffle(None));
        kb.insert("x".into(), Command::Share(TargetMode::Selected));
        kb.insert("Shift+x".into(), Command::Share(TargetMode::Current));

        kb.insert("F1".into(), Command::Focus("queue".into()));
        kb.insert("F2".into(), Command::Focus("search".into()));
        kb.insert("F3".into(), Command::Focus("library".into()));
        #[cfg(feature = "cover")]
        kb.insert("F8".into(), Command::Focus("cover".into()));
        kb.insert("?".into(), Command::Help);
        kb.insert("Backspace".into(), Command::Back);

        kb.insert("o".into(), Command::Open(TargetMode::Selected));
        kb.insert("Shift+o".into(), Command::Open(TargetMode::Current));
        kb.insert("a".into(), Command::Goto(GotoMode::Album));
        kb.insert("A".into(), Command::Goto(GotoMode::Artist));

        kb.insert("Up".into(), Command::Move(MoveMode::Up, Default::default()));
        kb.insert(
            "p".into(),
            Command::Move(MoveMode::Playing, Default::default()),
        );
        kb.insert(
            "Down".into(),
            Command::Move(MoveMode::Down, Default::default()),
        );
        kb.insert(
            "Left".into(),
            Command::Move(MoveMode::Left, Default::default()),
        );
        kb.insert(
            "Right".into(),
            Command::Move(MoveMode::Right, Default::default()),
        );
        kb.insert(
            "PageUp".into(),
            Command::Move(MoveMode::Up, MoveAmount::Integer(5)),
        );
        kb.insert(
            "PageDown".into(),
            Command::Move(MoveMode::Down, MoveAmount::Integer(5)),
        );
        kb.insert(
            "Home".into(),
            Command::Move(MoveMode::Up, MoveAmount::Extreme),
        );
        kb.insert(
            "End".into(),
            Command::Move(MoveMode::Down, MoveAmount::Extreme),
        );
        kb.insert("k".into(), Command::Move(MoveMode::Up, Default::default()));
        kb.insert(
            "j".into(),
            Command::Move(MoveMode::Down, Default::default()),
        );
        kb.insert(
            "h".into(),
            Command::Move(MoveMode::Left, Default::default()),
        );
        kb.insert(
            "l".into(),
            Command::Move(MoveMode::Right, Default::default()),
        );

        kb.insert(
            "Ctrl+p".into(),
            Command::Move(MoveMode::Up, Default::default()),
        );
        kb.insert(
            "Ctrl+n".into(),
            Command::Move(MoveMode::Down, Default::default()),
        );
        kb.insert(
            "Ctrl+a".into(),
            Command::Move(MoveMode::Left, Default::default()),
        );
        kb.insert(
            "Ctrl+e".into(),
            Command::Move(MoveMode::Right, Default::default()),
        );

        kb.insert("Shift+Up".into(), Command::Shift(ShiftMode::Up, None));
        kb.insert("Shift+Down".into(), Command::Shift(ShiftMode::Down, None));
        kb.insert("Ctrl+v".into(), Command::Insert(None));

        kb
    }

    fn parse_key(key: &str) -> Event {
        match key {
            "Enter" => Event::Key(Key::Enter),
            "Space" => Event::Char(" ".chars().next().unwrap()),
            "Tab" => Event::Key(Key::Tab),
            "Backspace" => Event::Key(Key::Backspace),
            "Esc" => Event::Key(Key::Esc),
            "Left" => Event::Key(Key::Left),
            "Right" => Event::Key(Key::Right),
            "Up" => Event::Key(Key::Up),
            "Down" => Event::Key(Key::Down),
            "Ins" => Event::Key(Key::Ins),
            "Del" => Event::Key(Key::Del),
            "Home" => Event::Key(Key::Home),
            "End" => Event::Key(Key::End),
            "PageUp" => Event::Key(Key::PageUp),
            "PageDown" => Event::Key(Key::PageDown),
            "PauseBreak" => Event::Key(Key::PauseBreak),
            "NumpadCenter" => Event::Key(Key::NumpadCenter),
            "F0" => Event::Key(Key::F0),
            "F1" => Event::Key(Key::F1),
            "F2" => Event::Key(Key::F2),
            "F3" => Event::Key(Key::F3),
            "F4" => Event::Key(Key::F4),
            "F5" => Event::Key(Key::F5),
            "F6" => Event::Key(Key::F6),
            "F7" => Event::Key(Key::F7),
            "F8" => Event::Key(Key::F8),
            "F9" => Event::Key(Key::F9),
            "F10" => Event::Key(Key::F10),
            "F11" => Event::Key(Key::F11),
            "F12" => Event::Key(Key::F12),
            s => Event::Char(s.chars().next().unwrap()),
        }
    }

    fn parse_keybinding(kb: &str) -> Option<cursive::event::Event> {
        let mut split = kb.split('+');
        if kb != "+" && split.clone().count() == 2 {
            let modifier = split.next().unwrap();
            let key = split.next().unwrap();
            let parsed = Self::parse_key(key);
            if let Event::Key(parsed) = parsed {
                match modifier {
                    "Shift" => Some(Event::Shift(parsed)),
                    "Alt" => Some(Event::Alt(parsed)),
                    "Ctrl" => Some(Event::Ctrl(parsed)),
                    _ => None,
                }
            } else if let Event::Char(parsed) = parsed {
                match modifier {
                    "Shift" => Some(Event::Char(parsed.to_uppercase().next().unwrap())),
                    "Alt" => Some(Event::AltChar(parsed)),
                    "Ctrl" => Some(Event::CtrlChar(parsed)),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            Some(Self::parse_key(&kb))
        }
    }
}
