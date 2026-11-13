//! A stick written by openDJ's own USB export reads back through the USB
//! import with its tracks, cues, grid and lists, in both of the formats
//! Pioneer players read.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use opendj_engine::{Hotcue, LibraryInfo, Source, Track, MEMORY_CUE};
use opendj_export::{export, List, Options};
use opendj_plugins::rekordbox::usb::{read_stick, Stick};

/// A 16-bit stereo WAV of `seconds` of a click on every beat at `bpm`.
fn write_wav(path: &Path, seconds: f64, bpm: f64) {
    let rate = 44_100u32;
    let frames = (seconds * rate as f64) as usize;
    let beat = (60.0 / bpm * rate as f64) as usize;
    let mut pcm = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let t = (i % beat) as f64 / rate as f64;
        let s = ((t * 2.0 * std::f64::consts::PI * 60.0).sin() * (-t * 20.0).exp() * 20_000.0) as i16;
        pcm.extend_from_slice(&s.to_le_bytes());
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(&pcm);
    std::fs::write(path, out).unwrap();
}

fn track(file: PathBuf, title: &str) -> Track {
    Track {
        id: 0,
        artist: "Test Artist".into(),
        title: title.into(),
        album: "Test Album".into(),
        genre: "Techno".into(),
        rating: 4,
        bpm: 128.0,
        key: "8A".into(),
        duration: 20.0,
        first_beat: 0.0,
        grid: Default::default(),
        source: Source::File(file),
        waveform: Arc::default(),
        audio: None,
        cover: None,
        info: LibraryInfo {
            hotcues: vec![
                Hotcue { number: 0, seconds: 1.875, name: "Intro".into(), end: None, color: Some(0x28E214) },
                Hotcue { number: 2, seconds: 7.5, name: String::new(), end: Some(9.375), color: None },
                Hotcue { number: MEMORY_CUE, seconds: 3.75, name: "Mix in".into(), end: None, color: None },
            ],
            ..Default::default()
        },
    }
}

fn round_trip(device_library: bool, one_library: bool) {
    let dir = std::env::temp_dir().join(format!(
        "opendj-usb-{}-{}",
        std::process::id(),
        if one_library { "onelib" } else { "pdb" }
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let music = dir.join("music");
    let stick = dir.join("STICK");
    std::fs::create_dir_all(&music).unwrap();
    std::fs::create_dir_all(&stick).unwrap();
    let a = music.join("a.wav");
    let b = music.join("b.wav");
    write_wav(&a, 20.0, 128.0);
    write_wav(&b, 20.0, 128.0);
    let tracks = vec![track(a, "First"), track(b, "Second")];
    let lists = vec![
        List { name: "Gigs".into(), parent: None, folder: true, tracks: vec![] },
        List { name: "Saturday".into(), parent: Some(0), folder: false, tracks: vec![1, 0] },
        List { name: "Warm-up".into(), parent: None, folder: false, tracks: vec![0] },
    ];
    let options = Options {
        root: stick.clone(),
        name: "STICK".into(),
        device_library,
        one_library,
        engine_dj: false,
        camelot: true,
    };
    let report = export(&tracks, &lists, &options, &mut |_| true).expect("export");
    assert_eq!(report.tracks, 2);

    let read = read_stick(
        &Stick { root: stick.clone(), name: "STICK".into() },
        &["playlists", "history"],
    )
    .expect("read the stick back");
    assert_eq!(read.tracks.len(), 2, "both tracks");
    let first = read.tracks.iter().find(|t| t.title == "First").expect("First");
    assert_eq!(first.artist, "Test Artist");
    assert_eq!(first.genre, "Techno");
    assert!((first.bpm - 128.0).abs() < 0.01, "bpm {}", first.bpm);
    match &first.source {
        Source::File(p) => assert!(p.starts_with(&stick) && p.is_file(), "{}", p.display()),
        other => panic!("not a file: {other:?}"),
    }
    // Hot cue A with its name and colour, the loop on C, the memory cue.
    let cues = &first.info.hotcues;
    let a_cue = cues.iter().find(|c| c.number == 0).expect("hot cue A");
    assert!((a_cue.seconds - 1.875).abs() < 0.01);
    assert_eq!(a_cue.name, "Intro");
    let c_cue = cues.iter().find(|c| c.number == 2).expect("hot cue C");
    assert!(c_cue.end.is_some_and(|e| (e - 9.375).abs() < 0.01), "loop {:?}", c_cue.end);
    assert!(cues.iter().any(|c| c.is_memory() && (c.seconds - 3.75).abs() < 0.01), "memory cue: {cues:?}");
    assert!(first.info.fixed_grid, "the stick's grid");

    let saturday = read.lists.iter().find(|l| l.name == "Saturday").expect("Saturday");
    assert_eq!(saturday.folders, vec!["Gigs".to_string()]);
    let titles: Vec<PathBuf> = saturday.entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(titles.len(), 2);
    assert!(titles[0].ends_with("b.wav") || titles[0].to_string_lossy().contains("b."), "order {titles:?}");
    assert!(read.lists.iter().any(|l| l.name == "Warm-up" && l.folders.is_empty()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn device_library_sticks_read_back() {
    round_trip(true, false);
}

#[test]
fn onelibrary_sticks_read_back() {
    round_trip(false, true);
}

/// Writes a stick to look at by hand: `OPENDJ_DEMO_MUSIC` holds files named
/// "Artist - Title.ext", written to `OPENDJ_DEMO_STICK` with a few lists in
/// both formats. Run with `cargo test --test usb_stick -- --ignored`.
#[test]
#[ignore]
fn make_demo_stick() {
    let (Some(music), Some(stick)) = (
        std::env::var_os("OPENDJ_DEMO_MUSIC"),
        std::env::var_os("OPENDJ_DEMO_STICK"),
    ) else {
        return;
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&music)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let tracks: Vec<Track> = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let stem = f.file_stem().unwrap().to_string_lossy().to_string();
            let (artist, title) = stem.split_once(" - ").unwrap_or(("", &stem));
            let mut t = track(f.clone(), title);
            t.artist = artist.to_string();
            t.album = "USB Demo".into();
            t.bpm = 0.0;
            t.key.clear();
            t.genre = ["Techno", "House", "Deep House"][i % 3].into();
            t.duration = 0.0;
            t
        })
        .collect();
    let all: Vec<usize> = (0..tracks.len()).collect();
    let lists = vec![
        List { name: "Club Night".into(), parent: None, folder: true, tracks: vec![] },
        List { name: "Warm-up".into(), parent: Some(0), folder: false, tracks: all.iter().copied().take(3).collect() },
        List { name: "Peak Time".into(), parent: Some(0), folder: false, tracks: all.iter().copied().skip(2).collect() },
        List { name: "All Tracks".into(), parent: None, folder: false, tracks: all },
    ];
    let root = PathBuf::from(stick);
    std::fs::create_dir_all(&root).unwrap();
    let options = Options {
        root,
        name: "DJ STICK".into(),
        device_library: true,
        one_library: true,
        engine_dj: false,
        camelot: true,
    };
    let report = export(&tracks, &lists, &options, &mut |_| true).expect("export");
    eprintln!("{report:?}");
}
