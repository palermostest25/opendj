//! rekordbox USB sticks and SD cards, as a standalone player reads them: the
//! tracks rekordbox (or openDJ's own USB export) put on the stick, with their
//! hot cues, memory cues, loops, beat grids and phrases, and the stick's
//! playlists (in their folders) and history.
//!
//! A stick holds its library in `PIONEER/rekordbox/`: OneLibrary
//! (`exportLibrary.db`, which the XDJ-AZ, CDJ-3000X and OPUS-QUAD read) and
//! or the Device Library (`export.pdb`, the CDJ-3000, XDJ-RX3 and older).
//! OneLibrary is read when the stick has it, else the Device Library. Each
//! track's analysis files (`PIONEER/USBANLZ/…`) hold its grid, cues and
//! phrases.
//!
//! [`RekordboxUsb`] is a library import like the others, with no file to
//! pick: it reads every stick plugged in now ([`sticks`]), each in a folder
//! of its own under **USB**, and the app imports again when one comes or
//! goes. The tracks play from the stick.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use opendj_engine::import::{ImportedEntry, ImportedList, ImportedListKind};
use opendj_engine::{Hotcue, LibraryInfo, Source, Track, MEMORY_CUE};
use rbox_anlz::Anlz;

use super::common::{self, Mood};
use super::{HISTORY, PLAYLISTS};
use crate::{ImportedLibrary, LibraryImport, ListGroup};

/// The import's id, kept as the source of the tracks it brings in.
pub const ID: &str = "rekordbox-usb";
/// What the DJ sees: the folder the sticks' lists go in.
pub const TITLE: &str = "USB";
/// The folder on a stick that holds its rekordbox library.
const LIBRARY_DIR: &str = "PIONEER/rekordbox";
const ONELIBRARY: &str = "exportLibrary.db";
const DEVICE_LIBRARY: &str = "export.pdb";

/// A stick or card with a rekordbox library on it.
#[derive(Clone, Debug, PartialEq)]
pub struct Stick {
    /// Where it is mounted: `E:\`, `/Volumes/DJ`, `/media/me/DJ`.
    pub root: PathBuf,
    /// Its name, for the DJ: the volume's name, or its drive letter.
    pub name: String,
}

impl Stick {
    fn library(&self) -> PathBuf {
        self.root.join(LIBRARY_DIR)
    }

    /// Which of the two databases it is read from: OneLibrary first.
    fn database(&self) -> Option<PathBuf> {
        let dir = self.library();
        [ONELIBRARY, DEVICE_LIBRARY]
            .iter()
            .map(|f| dir.join(f))
            .find(|p| p.is_file())
    }
}

/// The places a stick can be mounted: every drive but the system's on
/// Windows, `/Volumes` on macOS, the desktop's mount folders on Linux, and
/// `OPENDJ_USB` (paths split as `PATH` is), for testing.
fn mount_points() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::var_os("OPENDJ_USB")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    let children = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default()
    };
    if cfg!(windows) {
        // A: and B: are floppy drives where they exist, slow to ask; the
        // system drive holds no stick.
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        for letter in 'D'..='Z' {
            let drive = format!("{letter}:");
            if !drive.eq_ignore_ascii_case(&system) {
                out.push(PathBuf::from(format!("{drive}\\")));
            }
        }
    } else if cfg!(target_os = "macos") {
        out.extend(children(Path::new("/Volumes")));
    } else {
        let user = std::env::var("USER").unwrap_or_default();
        for dir in [
            PathBuf::from("/media").join(&user),
            PathBuf::from("/run/media").join(&user),
            PathBuf::from("/media"),
            PathBuf::from("/mnt"),
        ] {
            out.extend(children(&dir));
        }
    }
    out
}

/// A drive's volume label on Windows ("DJ STICK"), if it has one.
#[cfg(windows)]
fn volume_label(root: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let wide: Vec<u16> = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut name = [0u16; 261];
    // SAFETY: `wide` ends in a nul and `name` is as long as said; the
    // other outputs may be null, which asks for nothing.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let label = String::from_utf16_lossy(&name[..len]).trim().to_string();
    (!label.is_empty()).then_some(label)
}

#[cfg(not(windows))]
fn volume_label(_root: &Path) -> Option<String> {
    None
}

/// A stick's name from where it is mounted: its volume label on Windows,
/// else the folder's name, or failing both its drive ("USB (E:)").
fn name_of(root: &Path) -> String {
    if let Some(label) = volume_label(root) {
        return label;
    }
    match root.file_name().and_then(|n| n.to_str()) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => {
            let drive = root.to_string_lossy().trim_end_matches(['\\', '/']).to_string();
            format!("USB ({drive})")
        }
    }
}

/// The sticks plugged in now that hold a rekordbox library, each once.
pub fn sticks() -> Vec<Stick> {
    let mut out: Vec<Stick> = Vec::new();
    for root in mount_points() {
        let stick = Stick {
            name: name_of(&root),
            root,
        };
        if stick.database().is_some() && !out.iter().any(|s| s.root == stick.root) {
            out.push(stick);
        }
    }
    // Two sticks with the same name are told apart by their place.
    let names: Vec<String> = out.iter().map(|s| s.name.clone()).collect();
    for (i, s) in out.iter_mut().enumerate() {
        if names.iter().filter(|n| **n == names[i]).count() > 1 {
            s.name = format!("{} ({})", s.name, s.root.display());
        }
    }
    out
}

fn modified(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// What changes when a stick comes, goes or is exported to again.
pub fn stamp_of(sticks: &[Stick]) -> String {
    sticks
        .iter()
        .map(|s| {
            let db = s.database().unwrap_or_default();
            format!("{}:{}", s.root.display(), modified(&db))
        })
        .collect::<Vec<_>>()
        .join("|")
}

pub struct RekordboxUsb;

impl LibraryImport for RekordboxUsb {
    fn id(&self) -> &str {
        ID
    }

    fn title(&self) -> &str {
        TITLE
    }

    /// Always there: with no stick in, the import is empty, so a stick's
    /// lists go away when it does.
    fn database(&self) -> Option<PathBuf> {
        Some(PathBuf::from(TITLE))
    }

    fn list_groups(&self) -> Vec<ListGroup> {
        vec![
            ListGroup {
                id: PLAYLISTS,
                title: "Playlists",
                detail: "The stick's playlists, in their folders.",
                on_by_default: true,
            },
            ListGroup {
                id: HISTORY,
                title: "History",
                detail: "What was played from the stick.",
                on_by_default: true,
            },
        ]
    }

    fn stamp(&self, _database: &Path) -> String {
        stamp_of(&sticks())
    }

    fn read(&self, _database: &Path, groups: &[&str]) -> Result<ImportedLibrary, String> {
        let mut all = ImportedLibrary::default();
        for stick in sticks() {
            match read_stick(&stick, groups) {
                Ok(mut library) => {
                    eprintln!(
                        "USB {}: {} tracks, {} lists",
                        stick.name,
                        library.tracks.len(),
                        library.lists.len()
                    );
                    for list in &mut library.lists {
                        list.folders.insert(0, stick.name.clone());
                    }
                    all.tracks.append(&mut library.tracks);
                    all.lists.append(&mut library.lists);
                }
                Err(e) => eprintln!("can't read the USB stick {}: {e}", stick.name),
            }
        }
        Ok(all)
    }
}

/// One stick's tracks and lists, from OneLibrary or else the Device Library.
pub fn read_stick(stick: &Stick, groups: &[&str]) -> Result<ImportedLibrary, String> {
    let db = stick.database().ok_or("no rekordbox library on it")?;
    if db.file_name().is_some_and(|n| n == ONELIBRARY) {
        onelibrary::read(&stick.root, &db, groups)
    } else {
        device_library::read(&stick.root, &db, groups)
    }
}

/// A file on the stick from the path its library gives ("/Contents/…").
fn on_stick(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches(['/', '\\']))
}

fn text(s: &str) -> String {
    s.trim().to_string()
}

/// The beat grid, cues and phrases from a track's analysis files on the
/// stick (`.DAT`, and `.EXT` beside it with the colours, names and phrases).
fn analysis(root: &Path, dat: &str, t: &mut Track) {
    let dat = dat.trim();
    if dat.is_empty() {
        return;
    }
    let dat_path = on_stick(root, dat);
    let ext_path = dat_path.with_extension("EXT");
    let mut beats: Vec<(f64, f64)> = Vec::new();
    let mut cues: Vec<Hotcue> = Vec::new();
    if let Ok(mut f) = Anlz::load(&dat_path) {
        if let Ok(Some(g)) = f.get_beat_grid() {
            beats = g
                .beats
                .iter()
                .map(|b| (b.time as f64 / 1000.0, b.tempo as f64 / 100.0))
                .collect();
        }
        for list in [f.get_hot_cues().ok().flatten().cloned(), f.get_memory_cues().ok().flatten().cloned()]
            .into_iter()
            .flatten()
        {
            for c in &list.cues {
                cues.push(cue(c.hot_cue, c.time, c.loop_time, c.cue_type == rbox_anlz::CueType::Loop, String::new(), None));
            }
        }
    }
    let mut ext = Anlz::load(&ext_path).ok();
    if let Some(f) = ext.as_mut() {
        // The extended lists, with names and colours, take the place of
        // the plain ones.
        let mut named: Vec<Hotcue> = Vec::new();
        for list in [
            f.get_extended_hot_cues().ok().flatten().cloned(),
            f.get_extended_memory_cues().ok().flatten().cloned(),
        ]
        .into_iter()
        .flatten()
        {
            for c in &list.cues {
                let (r, g, b) = c.hot_cue_color_rgb;
                let colour = (c.hot_cue != 0 && (r, g, b) != (0, 0, 0))
                    .then(|| (r as u32) << 16 | (g as u32) << 8 | b as u32);
                named.push(cue(
                    c.hot_cue,
                    c.time,
                    c.loop_time,
                    c.cue_type == rbox_anlz::CueType::Loop,
                    c.comment.as_str().trim().to_string(),
                    colour,
                ));
            }
        }
        if !named.is_empty() {
            cues = named;
        }
    }
    if !cues.is_empty() {
        t.info.hotcues = number_memory_cues(cues);
    }
    if !beats.is_empty() {
        t.set_grid(common::grid_from_beats(&beats));
        t.info.fixed_grid = true;
        if t.bpm <= 0.0 {
            t.bpm = beats[0].1;
        }
    }
    if let Some(Ok(Some(s))) = ext.as_mut().map(|f| f.get_song_structure()) {
        let mood = match s.mood {
            rbox_anlz::Mood::High => Mood::High,
            rbox_anlz::Mood::Low => Mood::Low,
            _ => Mood::Mid,
        };
        let starts: Vec<f64> = beats.iter().map(|b| b.0).collect();
        let phrases: Vec<(u16, u16)> = s.phrases.iter().map(|p| (p.beat, p.kind)).collect();
        t.info.markers = common::phrases(mood, &phrases, s.end_beat, &starts);
    }
}

/// A cue from the analysis files: hot cue `hot` (1 is A; 0 a memory cue,
/// numbered later), at `time` ms, a loop to `loop_time` ms.
fn cue(hot: u32, time: u32, loop_time: u32, is_loop: bool, name: String, color: Option<u32>) -> Hotcue {
    let seconds = time as f64 / 1000.0;
    Hotcue {
        number: if hot == 0 { usize::MAX } else { hot as usize - 1 },
        seconds,
        name,
        end: (is_loop && loop_time > time).then(|| loop_time as f64 / 1000.0),
        color,
    }
}

/// Hot cues on their pads (one per pad), then the memory cues in time
/// order, numbered from [`MEMORY_CUE`].
fn number_memory_cues(mut cues: Vec<Hotcue>) -> Vec<Hotcue> {
    cues.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    let mut out: Vec<Hotcue> = Vec::new();
    let mut memory = MEMORY_CUE;
    for mut c in cues {
        if c.number == usize::MAX {
            c.number = memory;
            memory += 1;
        } else if out.iter().any(|h| h.number == c.number) {
            continue;
        }
        out.push(c);
    }
    out.sort_by_key(|h| h.number);
    out
}

/// A track with what the library says of it; the grid and cues come from
/// [`analysis`].
#[allow(clippy::too_many_arguments)]
fn track(
    file: PathBuf,
    title: String,
    artist: String,
    album: String,
    genre: String,
    key: String,
    bpm: f64,
    duration: f64,
    rating: i32,
    info: LibraryInfo,
) -> Track {
    Track {
        id: 0,
        artist,
        title,
        album,
        genre,
        rating: rating.clamp(0, 5) as u8,
        bpm,
        key,
        duration,
        first_beat: 0.0,
        grid: Default::default(),
        source: Source::File(file),
        waveform: Arc::default(),
        audio: None,
        cover: None,
        info,
    }
}

/// A list's entries from track ids, in order, skipping ids not on the stick.
fn entries(ids: impl IntoIterator<Item = i64>, paths: &HashMap<i64, PathBuf>) -> Vec<ImportedEntry> {
    ids.into_iter()
        .filter_map(|id| paths.get(&id))
        .map(|p| ImportedEntry::new(p.clone(), None))
        .collect()
}

/// Lists in tree order with the folders each sits in: `(id, parent, seq,
/// name, is_folder)` rows from either database, children of `root`.
fn tree(rows: &[(i64, i64, i64, String, bool)], root: i64) -> Vec<(i64, Vec<String>)> {
    let mut children: HashMap<i64, Vec<&(i64, i64, i64, String, bool)>> = HashMap::new();
    for r in rows {
        children.entry(r.1).or_default().push(r);
    }
    for c in children.values_mut() {
        c.sort_by_key(|r| r.2);
    }
    let mut out = Vec::new();
    let mut stack: Vec<(&(i64, i64, i64, String, bool), Vec<String>)> = children
        .get(&root)
        .map(|c| c.iter().rev().map(|r| (*r, Vec::new())).collect())
        .unwrap_or_default();
    while let Some((r, folders)) = stack.pop() {
        if r.4 {
            if folders.len() < 32 {
                let mut inside = folders.clone();
                inside.push(r.3.clone());
                if let Some(c) = children.get(&r.0) {
                    stack.extend(c.iter().rev().map(|c| (*c, inside.clone())));
                }
            }
        } else {
            out.push((r.0, folders));
        }
    }
    out
}

mod onelibrary {
    use super::*;
    use rbox_onelib::{HistoryType, OneLibrary, PlaylistType};

    fn err(e: impl std::fmt::Display) -> String {
        format!("can't read exportLibrary.db: {e}")
    }

    pub fn read(root: &Path, db: &Path, groups: &[&str]) -> Result<ImportedLibrary, String> {
        let lib = OneLibrary::from_path(db).map_err(err)?;
        let mut cues: HashMap<i64, Vec<rbox_onelib::Cue>> = HashMap::new();
        for c in lib.get_cues().map_err(err)? {
            cues.entry(c.content_id).or_default().push(c);
        }
        let mut tracks = Vec::new();
        let mut paths: HashMap<i64, PathBuf> = HashMap::new();
        for v in lib.get_contents_with_relations().map_err(err)? {
            let c = &v.content;
            let file = on_stick(root, &c.path);
            if !file.is_file() {
                continue;
            }
            paths.insert(c.id, file.clone());
            let name = |s: Option<&String>| s.map(|s| text(s)).unwrap_or_default();
            let mut t = track(
                file,
                name(c.title.as_ref()),
                name(v.artist.as_ref().map(|a| &a.name)),
                name(v.album.as_ref().map(|a| &a.name)),
                name(v.genre.as_ref().map(|g| &g.name)),
                name(v.key.as_ref().map(|k| &k.name)),
                c.bpmx100.unwrap_or(0) as f64 / 100.0,
                c.length.unwrap_or(0).max(0) as f64,
                c.rating.unwrap_or(0),
                LibraryInfo {
                    comment: name(c.dj_comment.as_ref()),
                    year: c
                        .release_year
                        .filter(|y| *y > 0)
                        .map(|y| y.to_string())
                        .unwrap_or_default(),
                    track_number: c.track_no.unwrap_or(0).max(0) as u32,
                    label: name(v.label.as_ref().map(|l| &l.name)),
                    remixer: name(v.remixer.as_ref().map(|a| &a.name)),
                    remix: name(c.subtitle.as_ref()),
                    isrc: name(c.isrc.as_ref()),
                    ..Default::default()
                },
            );
            analysis(root, c.analysis_data_file_path.as_deref().unwrap_or_default(), &mut t);
            // Cues kept in the database, for a stick whose analysis files
            // have none.
            if t.info.hotcues.is_empty() {
                let found: Vec<Hotcue> = cues
                    .remove(&c.id)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|q| {
                        let kind = q.kind.unwrap_or(0) as i64;
                        let number = if kind == 0 { Some(usize::MAX) } else { common::hot_cue_number(kind) }?;
                        let at = q.in_usec?;
                        Some(Hotcue {
                            number,
                            seconds: at as f64 / 1e6,
                            name: q.cue_comment.as_deref().map(text).unwrap_or_default(),
                            end: q.out_usec.filter(|o| *o > at).map(|o| o as f64 / 1e6),
                            color: q.color_table_index.and_then(|i| common::hot_cue_colour(i.into())),
                        })
                    })
                    .collect();
                t.info.hotcues = number_memory_cues(found);
            }
            tracks.push(t);
        }
        let mut lists = Vec::new();
        if groups.contains(&PLAYLISTS) {
            let rows: Vec<(i64, i64, i64, String, bool)> = lib
                .get_playlists()
                .map_err(err)?
                .into_iter()
                .filter(|p| matches!(p.attribute, PlaylistType::List | PlaylistType::Folder))
                .map(|p| (p.id, p.parent_id, p.seq as i64, p.name.clone(), matches!(p.attribute, PlaylistType::Folder)))
                .collect();
            let names: HashMap<i64, String> = rows.iter().map(|r| (r.0, r.3.clone())).collect();
            for (id, folders) in tree(&rows, 0) {
                let songs = lib.get_playlist_contents(id).map_err(err)?;
                lists.push(ImportedList {
                    folders,
                    name: names[&id].clone(),
                    kind: ImportedListKind::List,
                    entries: entries(songs.iter().map(|s| s.id), &paths),
                });
            }
        }
        if groups.contains(&HISTORY) {
            let rows: Vec<(i64, i64, i64, String, bool)> = lib
                .get_histories()
                .map_err(err)?
                .into_iter()
                .map(|h| (h.id, h.parent_id, h.seq as i64, h.name.clone(), matches!(h.attribute, HistoryType::Folder)))
                .collect();
            let names: HashMap<i64, String> = rows.iter().map(|r| (r.0, r.3.clone())).collect();
            for (id, folders) in tree(&rows, 0) {
                let songs = lib.get_history_contents(id).map_err(err)?;
                lists.push(ImportedList {
                    folders,
                    name: names[&id].clone(),
                    kind: ImportedListKind::History { started: 0 },
                    entries: entries(songs.iter().map(|s| s.id), &paths),
                });
            }
        }
        Ok(ImportedLibrary { tracks, lists })
    }
}

mod device_library {
    use super::*;
    use rbox_devlib::{DeviceLibrary, PlaylistType};

    fn err(e: impl std::fmt::Display) -> String {
        format!("can't read export.pdb: {e}")
    }

    pub fn read(root: &Path, db: &Path, groups: &[&str]) -> Result<ImportedLibrary, String> {
        let ext = db.with_file_name("exportExt.pdb");
        let mut lib = DeviceLibrary::from_paths(db, ext.is_file().then_some(ext.as_path())).map_err(err)?;
        let mut tracks = Vec::new();
        let mut paths: HashMap<i64, PathBuf> = HashMap::new();
        for v in lib.get_contents_with_relations().map_err(err)? {
            let c = &v.content;
            let file = on_stick(root, &c.path);
            if !file.is_file() {
                continue;
            }
            paths.insert(c.id, file.clone());
            let name = |s: Option<&String>| s.map(|s| text(s)).unwrap_or_default();
            let mut t = track(
                file,
                text(&c.title),
                name(v.artist.as_ref().map(|a| &a.name)),
                name(v.album.as_ref().map(|a| &a.name)),
                name(v.genre.as_ref().map(|g| &g.name)),
                name(v.key.as_ref().map(|k| &k.name)),
                c.bpmx100 as f64 / 100.0,
                c.length.max(0) as f64,
                c.rating,
                LibraryInfo {
                    comment: text(&c.comment),
                    year: (c.release_year > 0)
                        .then(|| c.release_year.to_string())
                        .unwrap_or_default(),
                    track_number: c.track_no.max(0) as u32,
                    disc_number: c.disc_no.max(0) as u32,
                    play_count: c.play_count.max(0) as u32,
                    label: name(v.label.as_ref().map(|l| &l.name)),
                    remixer: name(v.remixer.as_ref().map(|a| &a.name)),
                    remix: text(&c.mix_name),
                    color: common::track_colour(c.color_id),
                    ..Default::default()
                },
            );
            analysis(root, &c.analysis_path, &mut t);
            tracks.push(t);
        }
        let mut lists = Vec::new();
        if groups.contains(&PLAYLISTS) {
            let rows: Vec<(i64, i64, i64, String, bool)> = lib
                .get_playlists()
                .map_err(err)?
                .into_iter()
                .filter(|p| matches!(p.attribute, PlaylistType::List | PlaylistType::Folder))
                .map(|p| (p.id, p.parent_id, p.seq as i64, p.name.clone(), matches!(p.attribute, PlaylistType::Folder)))
                .collect();
            let names: HashMap<i64, String> = rows.iter().map(|r| (r.0, r.3.clone())).collect();
            for (id, folders) in tree(&rows, 0) {
                let songs = lib.get_playlist_contents(id).map_err(err)?;
                lists.push(ImportedList {
                    folders,
                    name: names[&id].clone(),
                    kind: ImportedListKind::List,
                    entries: entries(songs.iter().map(|s| s.id), &paths),
                });
            }
        }
        if groups.contains(&HISTORY) {
            let histories = lib.get_histories().map_err(err)?;
            for h in histories {
                let songs = lib.get_history_contents(h.id).map_err(err)?;
                lists.push(ImportedList {
                    folders: Vec::new(),
                    name: h.name.clone(),
                    kind: ImportedListKind::History { started: 0 },
                    entries: entries(songs.iter().map(|s| s.id), &paths),
                });
            }
        }
        Ok(ImportedLibrary { tracks, lists })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, parent: i64, seq: i64, name: &str, folder: bool) -> (i64, i64, i64, String, bool) {
        (id, parent, seq, name.to_string(), folder)
    }

    #[test]
    fn lists_come_in_tree_order_with_their_folders() {
        let rows = vec![
            row(1, 0, 2, "Gigs", true),
            row(2, 1, 1, "Saturday", false),
            row(3, 0, 1, "Warm-up", false),
            row(4, 1, 0, "Friday", false),
        ];
        let order: Vec<(i64, Vec<String>)> = tree(&rows, 0);
        assert_eq!(
            order,
            vec![
                (3, vec![]),
                (4, vec!["Gigs".to_string()]),
                (2, vec!["Gigs".to_string()]),
            ]
        );
    }

    #[test]
    fn hot_cues_keep_their_pads_and_memory_cues_follow() {
        let cues = vec![
            cue(0, 30_000, 0, false, String::new(), None),
            cue(2, 10_000, 0, false, "Drop".into(), Some(0xff0000)),
            cue(1, 1_000, 9_000, true, String::new(), None),
            cue(0, 5_000, 0, false, String::new(), None),
        ];
        let out = number_memory_cues(cues);
        let numbers: Vec<usize> = out.iter().map(|c| c.number).collect();
        assert_eq!(numbers, vec![0, 1, MEMORY_CUE, MEMORY_CUE + 1]);
        assert_eq!(out[0].end, Some(9.0));
        assert_eq!(out[1].name, "Drop");
        // Memory cues in time order.
        assert_eq!(out[2].seconds, 5.0);
    }

    #[test]
    fn files_are_found_from_the_stick_root() {
        let root = Path::new("/media/dj/STICK");
        assert_eq!(
            on_stick(root, "/Contents/A/B/track.mp3"),
            PathBuf::from("/media/dj/STICK/Contents/A/B/track.mp3")
        );
        assert_eq!(name_of(root), "STICK");
    }
}
