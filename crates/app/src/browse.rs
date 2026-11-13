//! The Standalone layout's Browse window (ui/browse.slint) and the
//! layout's settings (`StandaloneUi`): how many decks it shows
//! (`standalone_decks`), and the window, which opens when the main window
//! goes to Standalone and closes when it leaves.
//!
//! The window browses as a standalone player does. Its sources are the
//! collection and each rekordbox USB stick plugged in (read by
//! `opendj_plugins::rekordbox::usb`; [`watch_sticks`] notices sticks come
//! and go and has the library read them again). Its categories are
//! PLAYLIST, TRACK, ARTIST, ALBUM, GENRE, KEY, BPM, HISTORY and MATCHING:
//! each is either a list of lists to step into (playlists and folders,
//! artists, keys...) or tracks. Tracks are always the main window's track
//! list, which the window shares as a model: opening a playlist shows it
//! there, and an artist or a key is a search of the collection (only the
//! stick's tracks for a stick). So a controller's browse knob and LOAD
//! buttons work on what the window shows; in its lists of lists the knob
//! moves and opens here instead ([`knob`]).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use opendj_engine::key::Key;
use opendj_library::query::quoted;
use opendj_plugins::rekordbox::usb::{self, Stick};
use slint::{CloseRequestResponse, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::browser::{BrowseListKind, Browser, Scope};
use crate::{settings, AppWindow, BrowseItem, BrowseWindow, KeyNotation, StandaloneUi, Theme};

/// The Standalone layout's value of `layout` (main.slint).
pub(crate) const STANDALONE_LAYOUT: i32 = 5;
/// The setting for how many decks Standalone shows: 0 Automatic, 2 or 4.
const DECKS: &str = "standalone_decks";
/// How often to look for sticks coming and going.
const STICK_CHECK: Duration = Duration::from_secs(2);

/// A saved deck count made safe: 2 or 4, else Automatic (0).
fn decks(saved: f64) -> i32 {
    match saved.round() as i32 {
        n @ (2 | 4) => n,
        _ => 0,
    }
}

/// What a controller's browse knob and BACK do in the Browse window. The
/// browser takes the controls each frame (`Browser::poll_controllers`) and
/// hands them over here while the window is open; the window's timer acts
/// on them. Both run on the UI thread.
pub(crate) mod knob {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::Relaxed};

    static OPEN: AtomicBool = AtomicBool::new(false);
    static LISTS: AtomicBool = AtomicBool::new(false);
    static TURNS: AtomicI32 = AtomicI32::new(0);
    static PRESSES: AtomicU32 = AtomicU32::new(0);
    static BACKS: AtomicU32 = AtomicU32::new(0);

    /// The Browse window is open: it takes BACK.
    pub fn open() -> bool {
        OPEN.load(Relaxed)
    }
    /// It shows a list of lists: it takes the knob too.
    pub fn lists() -> bool {
        OPEN.load(Relaxed) && LISTS.load(Relaxed)
    }
    pub(super) fn set(open: bool, lists: bool) {
        OPEN.store(open, Relaxed);
        LISTS.store(lists, Relaxed);
    }
    pub fn turn(steps: i32) {
        TURNS.fetch_add(steps, Relaxed);
    }
    pub fn press() {
        PRESSES.fetch_add(1, Relaxed);
    }
    pub fn back() {
        BACKS.fetch_add(1, Relaxed);
    }
    /// The turns, presses and BACKs since last time.
    pub(super) fn take() -> (i32, u32, u32) {
        (TURNS.swap(0, Relaxed), PRESSES.swap(0, Relaxed), BACKS.swap(0, Relaxed))
    }
}

/// Looks for rekordbox sticks every couple of seconds on a thread of its
/// own (a drive that is slow to answer holds up nothing), and sends the
/// sticks whenever they change: once at the start, then on each change.
fn watch_sticks() -> mpsc::Receiver<Vec<Stick>> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("usb-sticks".into())
        .spawn(move || {
            let mut last: Option<String> = None;
            loop {
                let sticks = usb::sticks();
                let stamp = usb::stamp_of(&sticks);
                if last.as_ref() != Some(&stamp) {
                    last = Some(stamp);
                    if tx.send(sticks).is_err() {
                        return;
                    }
                }
                std::thread::sleep(STICK_CHECK);
            }
        });
    rx
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    Playlist,
    Track,
    Artist,
    Album,
    Genre,
    Key,
    Bpm,
    History,
    Matching,
}

const CATEGORIES: [(Category, &str); 9] = [
    (Category::Playlist, "PLAYLIST"),
    (Category::Track, "TRACK"),
    (Category::Artist, "ARTIST"),
    (Category::Album, "ALBUM"),
    (Category::Genre, "GENRE"),
    (Category::Key, "KEY"),
    (Category::Bpm, "BPM"),
    (Category::History, "HISTORY"),
    (Category::Matching, "MATCHING"),
];

/// What opening an item does.
#[derive(Clone, Debug, PartialEq)]
enum Open {
    /// Steps into a folder of lists.
    Folder(i64),
    /// Shows a list's tracks.
    List(i64),
    /// Shows the collection narrowed by a search.
    Search(String),
}

#[derive(Clone, Debug, PartialEq)]
struct Item {
    label: String,
    detail: String,
    icon: i32,
    wheel: i32,
    open: Open,
}

impl Item {
    fn ui(&self) -> BrowseItem {
        BrowseItem {
            label: self.label.clone().into(),
            detail: self.detail.clone().into(),
            icon: self.icon,
            opens: matches!(self.open, Open::Folder(_)),
            wheel: self.wheel,
        }
    }
}

#[derive(Clone, Debug)]
enum View {
    Items {
        title: String,
        items: Vec<Item>,
        current: i32,
        /// The folder shown, to list again when the library changes.
        folder: Option<i64>,
    },
    Tracks {
        title: String,
    },
}

struct Nav {
    sticks: Vec<Stick>,
    /// 0 the collection, then the sticks.
    source: usize,
    category: Category,
    stack: Vec<View>,
    search: String,
}

fn tracks_detail(n: usize) -> String {
    if n == 1 {
        "1 track".into()
    } else {
        format!("{n} tracks")
    }
}

impl Nav {
    fn scope(&self) -> Scope {
        match self.source.checked_sub(1).and_then(|i| self.sticks.get(i)) {
            Some(s) => Scope::Stick(s.name.clone()),
            None => Scope::Collection,
        }
    }

    fn root(&self) -> Option<PathBuf> {
        self.source
            .checked_sub(1)
            .and_then(|i| self.sticks.get(i))
            .map(|s| s.root.clone())
    }

    fn source_name(&self) -> String {
        self.source
            .checked_sub(1)
            .and_then(|i| self.sticks.get(i))
            .map_or_else(|| "Collection".to_string(), |s| s.name.clone())
    }

    /// Shows the source's tracks narrowed by `query`.
    fn show_search(&self, browser: &Rc<RefCell<Browser>>, query: &str) {
        browser
            .borrow_mut()
            .browse_show_search(&self.scope(), self.root().as_deref(), query.trim());
    }

    fn top(&self) -> Option<&View> {
        self.stack.last()
    }
}

/// The lists in `folder` (or at the top of the source) as items.
fn list_items(b: &Browser, scope: &Scope, folder: Option<i64>) -> Vec<Item> {
    b.browse_lists(scope, folder)
        .into_iter()
        .map(|l| Item {
            // A smart list's tracks are worked out when it shows.
            detail: match l.kind {
                BrowseListKind::Smart => "Smart list".to_string(),
                _ => tracks_detail(l.count),
            },
            icon: match l.kind {
                BrowseListKind::Folder => 0,
                BrowseListKind::List => 1,
                BrowseListKind::History => 2,
                BrowseListKind::Smart => 8,
                BrowseListKind::Crate => 9,
            },
            wheel: 0,
            open: match l.kind {
                BrowseListKind::Folder => Open::Folder(l.id),
                _ => Open::List(l.id),
            },
            label: l.name,
        })
        .collect()
}

/// A field's values among the source's tracks, as items that search for
/// them, with how many tracks have each.
fn value_items(b: &Browser, nav: &Nav, category: Category, camelot: bool) -> Vec<Item> {
    let tracks = b.browse_tracks(&nav.scope(), nav.root().as_deref());
    // Sorted by what the DJ reads: names alphabetically, keys round the
    // wheel, tempos up.
    let mut found: BTreeMap<(i64, String), (String, usize, i32, String)> = BTreeMap::new();
    for t in &tracks {
        let (sort, label, wheel, query) = match category {
            Category::Artist | Category::Album | Category::Genre => {
                let (value, field) = match category {
                    Category::Artist => (&t.artist, "artist"),
                    Category::Album => (&t.album, "album"),
                    _ => (&t.genre, "genre"),
                };
                let v = value.trim();
                if v.is_empty() {
                    continue;
                }
                (
                    (0, v.to_lowercase()),
                    v.to_string(),
                    0,
                    format!("{field}:={}", quoted(v)),
                )
            }
            Category::Key => {
                let Some(k) = Key::parse(&t.key) else { continue };
                let label = k.label(camelot);
                let wheel_label = k.label(true);
                let minor = wheel_label.ends_with('A');
                (
                    (k.camelot_number() as i64 * 2 + i64::from(!minor), String::new()),
                    label,
                    k.camelot_number() as i32,
                    format!("key:{}", quoted(&wheel_label)),
                )
            }
            Category::Bpm => {
                if t.bpm <= 0.0 {
                    continue;
                }
                let whole = t.bpm.round() as i64;
                (
                    (whole, String::new()),
                    format!("{whole} BPM"),
                    0,
                    format!("bpm:{}-{}", whole as f64 - 0.5, whole as f64 + 0.49),
                )
            }
            _ => continue,
        };
        let e = found.entry(sort).or_insert((label, 0, wheel, query));
        e.1 += 1;
    }
    let icon = match category {
        Category::Artist => 3,
        Category::Album => 4,
        Category::Genre => 5,
        Category::Key => 6,
        _ => 7,
    };
    found
        .into_values()
        .map(|(label, n, wheel, query)| Item {
            label,
            detail: tracks_detail(n),
            icon,
            wheel,
            open: Open::Search(query),
        })
        .collect()
}

/// History sessions as items, newest first.
fn history_items(b: &Browser, scope: &Scope) -> Vec<Item> {
    b.browse_history(scope)
        .into_iter()
        .map(|l| Item {
            detail: tracks_detail(l.count),
            icon: 2,
            wheel: 0,
            open: Open::List(l.id),
            label: l.name,
        })
        .collect()
}

type Slot = Rc<RefCell<Option<BrowseWindow>>>;

/// Wires the Standalone settings and the Browse window. Call once, after
/// the library is installed. `rescan` reads the library again (with the
/// sticks plugged in now).
pub fn install(app: &AppWindow, browser: Rc<RefCell<Browser>>, rescan: Box<dyn Fn()>) {
    let ui = app.global::<StandaloneUi>();
    ui.set_decks(decks(settings::number(DECKS, 0.0)));
    ui.on_decks_changed(|n| settings::set(DECKS, serde_json::json!(decks(n as f64))));

    let slot: Slot = Rc::default();
    let nav = Rc::new(RefCell::new(Nav {
        sticks: Vec::new(),
        source: 0,
        category: Category::Playlist,
        stack: Vec::new(),
        search: String::new(),
    }));
    let sticks = watch_sticks();
    let first_sticks = Cell::new(true);
    // Follows the layout, keeps the window in step while it shows, and
    // acts on the knob.
    let timer = Rc::new(Timer::default());
    {
        let (weak, slot, nav, browser) = (app.as_weak(), slot.clone(), nav.clone(), browser.clone());
        let last_layout = Cell::new(i32::MIN);
        let last_refresh = Cell::new(Instant::now());
        let last_count = Cell::new(usize::MAX);
        timer.start(TimerMode::Repeated, Duration::from_millis(40), move || {
            let Some(app) = weak.upgrade() else { return };
            // Sticks coming and going: read the library again.
            if let Ok(found) = sticks.try_recv() {
                let gone = {
                    let mut n = nav.borrow_mut();
                    let picked = n.source.checked_sub(1).and_then(|i| n.sticks.get(i)).map(|s| s.root.clone());
                    n.sticks = found;
                    let still = picked.as_ref().and_then(|r| n.sticks.iter().position(|s| &s.root == r));
                    match (picked, still) {
                        (Some(_), None) => {
                            n.source = 0;
                            true
                        }
                        (Some(_), Some(i)) => {
                            n.source = i + 1;
                            false
                        }
                        _ => false,
                    }
                };
                if !first_sticks.replace(false) {
                    rescan();
                }
                if gone {
                    reset(&app, &browser, &nav);
                }
                if let Some(b) = slot.borrow().as_ref() {
                    show(&app, b, &nav.borrow());
                }
            }
            let layout = app.get_layout();
            if last_layout.replace(layout) != layout {
                let open = layout == STANDALONE_LAYOUT;
                app.global::<StandaloneUi>().set_browse_open(open);
                set_open(&app, &slot, &nav, &browser, open);
            }
            let visible = slot.borrow().as_ref().is_some_and(|b| b.window().is_visible());
            let lists = matches!(nav.borrow().top(), Some(View::Items { .. }));
            knob::set(visible, lists);
            if !visible {
                return;
            }
            let (turns, presses, backs) = knob::take();
            if turns != 0 {
                move_item(&nav, turns);
            }
            for _ in 0..presses {
                open_current(&app, &browser, &nav);
            }
            for _ in 0..backs {
                back(&app, &browser, &nav);
            }
            // A list of lists is listed again when the library changes (a
            // stick read, a list made), at most every two seconds.
            let count = browser.borrow().browse_library_len();
            if count != last_count.get() || last_refresh.get().elapsed() > Duration::from_secs(2) {
                if count != last_count.replace(count) || lists {
                    relist(&app, &browser, &nav);
                }
                last_refresh.set(Instant::now());
            }
            if let Some(b) = slot.borrow().as_ref() {
                show(&app, b, &nav.borrow());
            }
        });
    }
    let (weak, slot2, nav2, browser2) = (app.as_weak(), slot.clone(), nav.clone(), browser.clone());
    ui.on_browse_toggled(move |open| {
        // Holds the timer for as long as the main window lives.
        let _ = &timer;
        if let Some(app) = weak.upgrade() {
            set_open(&app, &slot2, &nav2, &browser2, open);
        }
    });
}

/// Back to the category's own first view, for the source picked.
fn reset(app: &AppWindow, browser: &Rc<RefCell<Browser>>, nav: &Rc<RefCell<Nav>>) {
    let category = nav.borrow().category;
    nav.borrow_mut().stack.clear();
    let camelot = app.global::<KeyNotation>().get_camelot();
    let view = {
        let n = nav.borrow();
        let b = browser.borrow();
        let scope = n.scope();
        match category {
            Category::Playlist => Some(list_items(&b, &scope, None)),
            Category::History => Some(history_items(&b, &scope)),
            Category::Artist | Category::Album | Category::Genre | Category::Key | Category::Bpm => {
                Some(value_items(&b, &n, category, camelot))
            }
            Category::Track | Category::Matching => None,
        }
    };
    let title = match category {
        Category::Playlist => "Playlists",
        Category::Track => "All tracks",
        Category::Artist => "Artists",
        Category::Album => "Albums",
        Category::Genre => "Genres",
        Category::Key => "Keys",
        Category::Bpm => "Tempos",
        Category::History => "History",
        Category::Matching => "Mixes with what plays now",
    }
    .to_string();
    match view {
        Some(items) => {
            app.set_current_track(-1);
            nav.borrow_mut().stack.push(View::Items {
                title,
                items,
                current: 0,
                folder: None,
            })
        }
        None => {
            let query = match category {
                Category::Matching => "~deck",
                _ => "",
            };
            nav.borrow().show_search(browser, query);
            nav.borrow_mut().stack.push(View::Tracks { title });
        }
    }
}

/// Lists the list of lists on top again, keeping the place in it.
fn relist(app: &AppWindow, browser: &Rc<RefCell<Browser>>, nav: &Rc<RefCell<Nav>>) {
    let camelot = app.global::<KeyNotation>().get_camelot();
    let mut n = nav.borrow_mut();
    let category = n.category;
    let depth = n.stack.len();
    let fresh = {
        let b = browser.borrow();
        let scope = n.scope();
        match n.stack.last() {
            Some(View::Items { folder, .. }) => match (category, folder) {
                (_, Some(f)) => Some(list_items(&b, &scope, Some(*f))),
                (Category::Playlist, None) => Some(list_items(&b, &scope, None)),
                (Category::History, None) => Some(history_items(&b, &scope)),
                (Category::Track | Category::Matching, None) => None,
                (c, None) if depth == 1 => Some(value_items(&b, &n, c, camelot)),
                _ => None,
            },
            _ => None,
        }
    };
    if let (Some(fresh), Some(View::Items { items, current, .. })) = (fresh, n.stack.last_mut()) {
        if *items != fresh {
            *current = (*current).clamp(0, (fresh.len() as i32 - 1).max(0));
            *items = fresh;
        }
    }
}

fn move_item(nav: &Rc<RefCell<Nav>>, by: i32) {
    if let Some(View::Items { items, current, .. }) = nav.borrow_mut().stack.last_mut() {
        if !items.is_empty() {
            *current = (*current + by).clamp(0, items.len() as i32 - 1);
        }
    }
}

/// Opens the item picked in the list of lists on top.
fn open_current(app: &AppWindow, browser: &Rc<RefCell<Browser>>, nav: &Rc<RefCell<Nav>>) {
    let item = match nav.borrow().top() {
        Some(View::Items { items, current, .. }) => items.get(*current as usize).cloned(),
        _ => None,
    };
    let Some(item) = item else { return };
    match item.open {
        Open::Folder(id) => {
            let items = list_items(&browser.borrow(), &nav.borrow().scope(), Some(id));
            nav.borrow_mut().stack.push(View::Items {
                title: item.label,
                items,
                current: 0,
                folder: Some(id),
            });
        }
        Open::List(id) => {
            browser.borrow_mut().browse_show_list(id);
            nav.borrow_mut().stack.push(View::Tracks { title: item.label });
        }
        Open::Search(query) => {
            nav.borrow().show_search(browser, &query);
            nav.borrow_mut().stack.push(View::Tracks { title: item.label });
        }
    }
    app.set_current_track(-1);
}

/// One step back: out of tracks to the list they came from, out of a
/// folder to the one holding it.
fn back(app: &AppWindow, browser: &Rc<RefCell<Browser>>, nav: &Rc<RefCell<Nav>>) {
    if nav.borrow().stack.len() > 1 {
        nav.borrow_mut().stack.pop();
        app.set_current_track(-1);
        relist(app, browser, nav);
    } else if !nav.borrow().search.is_empty() {
        nav.borrow_mut().search.clear();
        reset(app, browser, nav);
    }
}

/// Opens (making it the first time) or hides the Browse window.
fn set_open(app: &AppWindow, slot: &Slot, nav: &Rc<RefCell<Nav>>, browser: &Rc<RefCell<Browser>>, open: bool) {
    if !open {
        if let Some(b) = slot.borrow().as_ref() {
            let _ = b.hide();
        }
        knob::set(false, false);
        return;
    }
    if slot.borrow().is_none() {
        match BrowseWindow::new() {
            Ok(b) => {
                wire(app, &b, nav, browser);
                *slot.borrow_mut() = Some(b);
            }
            Err(e) => {
                eprintln!("can't open the Browse window: {e}");
                app.global::<StandaloneUi>().set_browse_open(false);
                return;
            }
        }
    }
    if nav.borrow().stack.is_empty() {
        reset(app, browser, nav);
    }
    if let Some(b) = slot.borrow().as_ref() {
        show(app, b, &nav.borrow());
        if let Err(e) = b.show() {
            eprintln!("can't show the Browse window: {e}");
        }
    }
}

/// Runs the Browse window's buttons.
fn wire(app: &AppWindow, b: &BrowseWindow, nav: &Rc<RefCell<Nav>>, browser: &Rc<RefCell<Browser>>) {
    let redraw = {
        let (a, w, nav) = (app.as_weak(), b.as_weak(), nav.clone());
        move || {
            if let (Some(app), Some(b)) = (a.upgrade(), w.upgrade()) {
                show(&app, &b, &nav.borrow());
            }
        }
    };
    let (a, n, br, r) = (app.as_weak(), nav.clone(), browser.clone(), redraw.clone());
    b.on_pick_source(move |i| {
        if let Some(app) = a.upgrade() {
            n.borrow_mut().source = i.max(0) as usize;
            reset(&app, &br, &n);
            r();
        }
    });
    let (a, n, br, r) = (app.as_weak(), nav.clone(), browser.clone(), redraw.clone());
    b.on_pick_category(move |i| {
        if let (Some(app), Some(&(c, _))) = (a.upgrade(), CATEGORIES.get(i.max(0) as usize)) {
            n.borrow_mut().category = c;
            n.borrow_mut().search.clear();
            reset(&app, &br, &n);
            r();
        }
    });
    let (a, n, br, r) = (app.as_weak(), nav.clone(), browser.clone(), redraw.clone());
    b.on_back(move || {
        if let Some(app) = a.upgrade() {
            back(&app, &br, &n);
            r();
        }
    });
    let (n, r) = (nav.clone(), redraw.clone());
    b.on_pick_item(move |i| {
        if let Some(View::Items { items, current, .. }) = n.borrow_mut().stack.last_mut() {
            *current = i.clamp(0, (items.len() as i32 - 1).max(0));
        }
        r();
    });
    let (a, n, br, r) = (app.as_weak(), nav.clone(), browser.clone(), redraw.clone());
    b.on_open_item(move |i| {
        if let Some(app) = a.upgrade() {
            if let Some(View::Items { current, .. }) = n.borrow_mut().stack.last_mut() {
                *current = i;
            }
            open_current(&app, &br, &n);
            r();
        }
    });
    // Search: the source's tracks matching what is typed, in place of the
    // category's view; clearing it goes back to the category.
    let (a, n, br, r) = (app.as_weak(), nav.clone(), browser.clone(), redraw.clone());
    b.on_search(move |text| {
        let Some(app) = a.upgrade() else { return };
        let text = text.trim().to_string();
        n.borrow_mut().search = text.clone();
        if text.is_empty() {
            reset(&app, &br, &n);
        } else {
            n.borrow().show_search(&br, &text);
            let mut nav = n.borrow_mut();
            // One search view at a time, over the category's first view.
            nav.stack.truncate(1);
            if matches!(nav.stack.first(), Some(View::Tracks { .. })) {
                nav.stack.clear();
            }
            nav.stack.push(View::Tracks { title: format!("“{text}”") });
        }
        r();
    });
    let a = app.as_weak();
    b.on_sort(move |c| {
        if let Some(app) = a.upgrade() {
            app.invoke_sort(c);
        }
    });
    let a = app.as_weak();
    b.on_pick(move |row| {
        if let Some(app) = a.upgrade() {
            app.set_folders_focused(false);
            app.set_current_track(row);
        }
    });
    let a = app.as_weak();
    b.on_activate(move |row| {
        if let Some(app) = a.upgrade() {
            app.invoke_activate_track(row);
        }
    });
    let a = app.as_weak();
    b.on_load(move |deck, row| {
        if let Some(app) = a.upgrade() {
            app.invoke_load_track(deck, row);
        }
    });
    // Closing it is the same as BROWSE in the top bar: it hides, and BROWSE
    // brings it back.
    let a = app.as_weak();
    b.window().on_close_requested(move || {
        if let Some(app) = a.upgrade() {
            app.global::<StandaloneUi>().set_browse_open(false);
        }
        knob::set(false, false);
        CloseRequestResponse::HideWindow
    });
}

/// Copies what the Browse window shows into it. The track model is the
/// main window's own, handed over only when the library swaps it.
fn show(app: &AppWindow, b: &BrowseWindow, nav: &Nav) {
    let mut sources: Vec<SharedString> = vec!["Collection".into()];
    sources.extend(nav.sticks.iter().map(|s| SharedString::from(s.name.as_str())));
    let sources_model = b.get_sources();
    if sources_model.row_count() != sources.len()
        || sources.iter().enumerate().any(|(i, s)| sources_model.row_data(i).as_ref() != Some(s))
    {
        b.set_sources(ModelRc::new(VecModel::from(sources)));
    }
    b.set_source(nav.source as i32);
    b.set_category(CATEGORIES.iter().position(|c| c.0 == nav.category).unwrap_or(0) as i32);
    let category = CATEGORIES.iter().find(|c| c.0 == nav.category).map_or("", |c| c.1);
    // "USB STICK › PLAYLIST › Gigs": the source, category and the lists
    // stepped through before the one shown.
    let searching = !nav.search.is_empty() && matches!(nav.top(), Some(View::Tracks { .. }));
    let category = if searching { "SEARCH" } else { category };
    let mut path = vec![nav.source_name().to_uppercase(), category.to_string()];
    let depth = nav.stack.len();
    for v in nav.stack.iter().take(depth.saturating_sub(1)).skip(1) {
        let (View::Items { title, .. } | View::Tracks { title }) = v;
        path.push(title.clone());
    }
    b.set_path(path.join("  ›  ").into());
    let (heading, mode) = match nav.top() {
        Some(View::Items { title, .. }) => (title.clone(), 0),
        Some(View::Tracks { title }) => (title.clone(), 1),
        None => (String::new(), 0),
    };
    b.set_heading(heading.into());
    b.set_mode(mode);
    b.set_can_back(nav.stack.len() > 1 || !nav.search.is_empty());
    if let Some(View::Items { items, current, .. }) = nav.top() {
        let model = b.get_items();
        let ui: Vec<BrowseItem> = items.iter().map(Item::ui).collect();
        if model.row_count() != ui.len() || ui.iter().enumerate().any(|(i, it)| model.row_data(i).as_ref() != Some(it)) {
            b.set_items(ModelRc::new(VecModel::from(ui)));
        }
        b.set_item_current(*current);
        b.set_summary(
            format!(
                "{} {}",
                items.len(),
                if items.len() == 1 { "item" } else { "items" }
            )
            .into(),
        );
    } else {
        let tracks = app.get_tracks();
        let duration = app.get_folder_duration();
        b.set_summary(
            if duration.is_empty() {
                tracks_detail(tracks.row_count())
            } else {
                format!("{} · {duration}", tracks_detail(tracks.row_count()))
            }
            .into(),
        );
    }
    let tracks = app.get_tracks();
    if b.get_tracks() != tracks {
        b.set_tracks(tracks);
    }
    b.set_current(app.get_current_track());
    b.set_sort_column(app.get_sort_column());
    b.set_sort_ascending(app.get_sort_ascending());
    b.set_decks_shown(if app.get_wave_four() { 4 } else { 2 });
    b.set_light(app.global::<Theme>().get_light());
}

use slint::Model;

#[cfg(test)]
mod tests {
    use super::decks;

    #[test]
    fn deck_counts_are_two_four_or_automatic() {
        assert_eq!(decks(2.0), 2);
        assert_eq!(decks(4.0), 4);
        assert_eq!(decks(0.0), 0);
        assert_eq!(decks(3.0), 0);
        assert_eq!(decks(-1.0), 0);
    }
}
