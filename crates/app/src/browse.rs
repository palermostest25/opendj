//! The Standalone layout's Browse window (ui/browse.slint) and the
//! layout's settings (`StandaloneUi`): how many decks it shows
//! (`standalone_decks`), and the window, which opens when the main window
//! goes to Standalone and closes when it leaves.
//!
//! The two windows share no globals, but they can share models: the Browse
//! window shows the main window's own folder and track models, so it stays
//! in step with the library as it refreshes, and a timer copies the
//! selection, the folder's name and the theme across. Its buttons run the
//! main window's callbacks, so loading from it is loading from the library,
//! and a controller's browse knob (which moves the main window's selection)
//! moves the Browse window's too.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use slint::{CloseRequestResponse, ComponentHandle, Timer, TimerMode};

use crate::{settings, AppWindow, BrowseWindow, StandaloneUi, Theme};

/// The Standalone layout's value of `layout` (main.slint).
pub(crate) const STANDALONE_LAYOUT: i32 = 5;
/// The setting for how many decks Standalone shows: 0 Automatic, 2 or 4.
const DECKS: &str = "standalone_decks";

/// A saved deck count made safe: 2 or 4, else Automatic (0).
fn decks(saved: f64) -> i32 {
    match saved.round() as i32 {
        n @ (2 | 4) => n,
        _ => 0,
    }
}

type Slot = Rc<RefCell<Option<BrowseWindow>>>;

/// Wires the Standalone settings and the Browse window. Call once, after
/// the library is installed.
pub fn install(app: &AppWindow) {
    let ui = app.global::<StandaloneUi>();
    ui.set_decks(decks(settings::number(DECKS, 0.0)));
    ui.on_decks_changed(|n| settings::set(DECKS, serde_json::json!(decks(n as f64))));

    let slot: Slot = Rc::default();
    // Follows the layout, and keeps the window in step while it shows.
    let timer = Rc::new(Timer::default());
    {
        let (weak, slot) = (app.as_weak(), slot.clone());
        let last_layout = Cell::new(i32::MIN);
        timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(app) = weak.upgrade() else { return };
            let layout = app.get_layout();
            if last_layout.replace(layout) != layout {
                let open = layout == STANDALONE_LAYOUT;
                app.global::<StandaloneUi>().set_browse_open(open);
                set_open(&app, &slot, open);
            }
            if let Some(b) = slot.borrow().as_ref() {
                if b.window().is_visible() {
                    sync(&app, b);
                }
            }
        });
    }
    let weak = app.as_weak();
    ui.on_browse_toggled(move |open| {
        // Holds the timer for as long as the main window lives.
        let _ = &timer;
        if let Some(app) = weak.upgrade() {
            set_open(&app, &slot, open);
        }
    });
}

/// Opens (making it the first time) or hides the Browse window.
fn set_open(app: &AppWindow, slot: &Slot, open: bool) {
    if !open {
        if let Some(b) = slot.borrow().as_ref() {
            let _ = b.hide();
        }
        return;
    }
    if slot.borrow().is_none() {
        match BrowseWindow::new() {
            Ok(b) => {
                wire(app, &b);
                *slot.borrow_mut() = Some(b);
            }
            Err(e) => {
                eprintln!("can't open the Browse window: {e}");
                app.global::<StandaloneUi>().set_browse_open(false);
                return;
            }
        }
    }
    if let Some(b) = slot.borrow().as_ref() {
        sync(app, b);
        if let Err(e) = b.show() {
            eprintln!("can't show the Browse window: {e}");
        }
    }
}

/// Runs the Browse window's buttons on the main window.
fn wire(app: &AppWindow, b: &BrowseWindow) {
    let a = app.as_weak();
    b.on_select_folder(move |i| {
        if let Some(app) = a.upgrade() {
            app.invoke_select_folder(i);
            // The browse knob goes on in the tracks.
            app.set_folders_focused(false);
        }
    });
    let a = app.as_weak();
    b.on_toggle_folder(move |i| {
        if let Some(app) = a.upgrade() {
            app.invoke_toggle_folder(i);
        }
    });
    let a = app.as_weak();
    b.on_search(move |text| {
        if let Some(app) = a.upgrade() {
            app.invoke_search(text);
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
        CloseRequestResponse::HideWindow
    });
}

/// Copies what the main window shows into the Browse window. The models
/// are the main window's own, so they are only handed over when the
/// library swaps one for another.
fn sync(app: &AppWindow, b: &BrowseWindow) {
    let folders = app.get_folders();
    if b.get_folders() != folders {
        b.set_folders(folders);
    }
    let tracks = app.get_tracks();
    if b.get_tracks() != tracks {
        b.set_tracks(tracks);
    }
    b.set_current_folder(app.get_current_folder());
    b.set_current(app.get_current_track());
    b.set_folder_name(app.get_folder_name());
    b.set_folder_duration(app.get_folder_duration());
    b.set_decks_shown(if app.get_wave_four() { 4 } else { 2 });
    b.set_light(app.global::<Theme>().get_light());
}

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
