//! What the Standalone layout's Browse window (`crate::browse`) asks of the
//! browser: the lists to step into, as a standalone player's PLAYLIST and
//! HISTORY show them, the tracks to work out ARTIST, ALBUM, GENRE, KEY and
//! BPM from, and showing a list or a search in the track list, which the
//! Browse window shares.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use opendj_engine::{Source, Track};
use opendj_library::PlaylistKind;
use opendj_plugins::rekordbox::usb;
use slint::ComponentHandle;

use super::Browser;

/// A list or folder for the Browse window.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowseList {
    pub id: i64,
    pub name: String,
    pub kind: BrowseListKind,
    /// Tracks in it; folders count what they hold.
    pub count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowseListKind {
    Folder,
    List,
    Smart,
    Crate,
    History,
}

/// Where the Browse window looks: the collection, or one USB stick (by the
/// name its folder has under USB).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Collection,
    Stick(String),
}

impl Browser {
    fn all_lists(&self) -> (Vec<opendj_library::Playlist>, HashMap<i64, usize>) {
        let Some(db) = self.db.as_ref() else {
            return (Vec::new(), HashMap::new());
        };
        (
            db.playlists().unwrap_or_default(),
            db.list_sizes().unwrap_or_default(),
        )
    }

    /// The folder a stick's lists are in: USB › its name.
    fn stick_folder(lists: &[opendj_library::Playlist], name: &str) -> Option<i64> {
        let top = lists.iter().find(|p| {
            p.parent.is_none()
                && p.source.as_deref() == Some(usb::ID)
                && p.kind == PlaylistKind::Folder
        })?;
        lists
            .iter()
            .find(|p| p.parent == Some(top.id) && p.name == name)
            .map(|p| p.id)
    }

    /// The lists and folders in `parent` (None: the top of `scope`), in the
    /// tree's order, without history sessions. The collection's top leaves
    /// the USB sticks' lists out: they have a source of their own.
    pub(crate) fn browse_lists(&self, scope: &Scope, parent: Option<i64>) -> Vec<BrowseList> {
        let (lists, sizes) = self.all_lists();
        let parent = match (scope, parent) {
            (_, Some(p)) => Some(p),
            (Scope::Collection, None) => None,
            (Scope::Stick(name), None) => match Self::stick_folder(&lists, name) {
                Some(id) => Some(id),
                None => return Vec::new(),
            },
        };
        let mut children: HashMap<Option<i64>, Vec<&opendj_library::Playlist>> = HashMap::new();
        for p in &lists {
            children.entry(p.parent).or_default().push(p);
        }
        // What a folder holds, counted down through its folders.
        fn held(
            id: i64,
            children: &HashMap<Option<i64>, Vec<&opendj_library::Playlist>>,
            sizes: &HashMap<i64, usize>,
            depth: usize,
        ) -> usize {
            if depth > 32 {
                return 0;
            }
            children.get(&Some(id)).map_or(0, |c| {
                c.iter()
                    .map(|p| match p.kind {
                        PlaylistKind::Folder | PlaylistKind::Set => held(p.id, children, sizes, depth + 1),
                        _ => sizes.get(&p.id).copied().unwrap_or(0),
                    })
                    .sum()
            })
        }
        children
            .get(&parent)
            .map(|c| c.as_slice())
            .unwrap_or_default()
            .iter()
            .filter(|p| p.kind != PlaylistKind::History)
            .filter(|p| !(parent.is_none() && p.source.as_deref() == Some(usb::ID)))
            .map(|p| {
                let kind = match p.kind {
                    PlaylistKind::Folder | PlaylistKind::Set => BrowseListKind::Folder,
                    PlaylistKind::Smart => BrowseListKind::Smart,
                    PlaylistKind::History => BrowseListKind::History,
                    PlaylistKind::List => BrowseListKind::List,
                    _ => BrowseListKind::Crate,
                };
                let count = match kind {
                    BrowseListKind::Folder => held(p.id, &children, &sizes, 0),
                    _ => sizes.get(&p.id).copied().unwrap_or(0),
                };
                BrowseList {
                    id: p.id,
                    name: p.name.clone(),
                    kind,
                    count,
                }
            })
            .collect()
    }

    /// History sessions, newest first: the DJ's own and other apps' for the
    /// collection, a stick's for a stick.
    pub(crate) fn browse_history(&self, scope: &Scope) -> Vec<BrowseList> {
        let (lists, sizes) = self.all_lists();
        let mut out: Vec<(i64, BrowseList)> = lists
            .iter()
            .filter(|p| p.kind == PlaylistKind::History)
            .filter(|p| match scope {
                Scope::Collection => p.source.as_deref() != Some(usb::ID),
                Scope::Stick(_) => p.source.as_deref() == Some(usb::ID),
            })
            .map(|p| {
                (
                    p.created,
                    BrowseList {
                        id: p.id,
                        name: p.name.clone(),
                        kind: BrowseListKind::History,
                        count: sizes.get(&p.id).copied().unwrap_or(0),
                    },
                )
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        out.into_iter().map(|(_, l)| l).collect()
    }

    /// The tracks on a stick, by place in `library`: those in its lists
    /// (a rekordbox export puts tracks on a stick by their lists), and any
    /// played from the stick itself. A stick's track the collection already
    /// had stands for its copy there, so a path alone can't say.
    fn stick_tracks(&self, name: &str, root: Option<&Path>) -> HashSet<usize> {
        let mut out: HashSet<usize> = HashSet::new();
        if let Some(root) = root {
            out.extend(self.library.iter().enumerate().filter_map(|(i, t)| match &t.source {
                Source::File(p) if p.starts_with(root) => Some(i),
                _ => None,
            }));
        }
        let (lists, _) = self.all_lists();
        let Some(top) = Self::stick_folder(&lists, name) else {
            return out;
        };
        let Some(db) = self.db.as_ref() else {
            return out;
        };
        let mut folders = vec![top];
        while let Some(f) = folders.pop() {
            for p in lists.iter().filter(|p| p.parent == Some(f)) {
                match p.kind {
                    PlaylistKind::Folder | PlaylistKind::Set => folders.push(p.id),
                    _ => {
                        for id in db.playlist_tracks(p.id).unwrap_or_default() {
                            if let Some(&i) = self.index_of.get(&id) {
                                out.insert(i);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// How many tracks the collection has, to notice it change.
    pub(crate) fn browse_library_len(&self) -> usize {
        self.library.len()
    }

    /// The collection's tracks, or a stick's.
    pub(crate) fn browse_tracks(&self, scope: &Scope, root: Option<&Path>) -> Vec<Arc<Track>> {
        match scope {
            Scope::Collection => self.library.clone(),
            Scope::Stick(name) => {
                let only = self.stick_tracks(name, root);
                let mut ids: Vec<usize> = only.into_iter().collect();
                ids.sort_unstable();
                ids.into_iter().map(|i| self.library[i].clone()).collect()
            }
        }
    }

    /// Shows list `id` in the track list: a history session in the history
    /// tab, any other list in the collection's tree.
    pub(crate) fn browse_show_list(&mut self, id: i64) {
        // A search typed before, or a stick's, would narrow the list.
        self.search.clear();
        self.browse_only = None;
        let history = self
            .nodes
            .iter()
            .any(|n| n.history && n.playlist == Some(id));
        if !history {
            self.show_list(id);
            return;
        }
        self.leave_for_collection();
        self.close_page();
        self.show_history(true);
        if let Some(app) = self.app.upgrade() {
            app.global::<crate::LibraryEdit>().set_history_tab(true);
        }
        if let Some(n) = self.nodes.iter().position(|n| n.playlist == Some(id)) {
            self.current_node = n;
        }
        self.after_jump();
    }

    /// Shows the collection narrowed by `query` (openDJ's search): for a
    /// stick, only the stick's tracks.
    pub(crate) fn browse_show_search(&mut self, scope: &Scope, root: Option<&Path>, query: &str) {
        self.leave_for_collection();
        self.close_page();
        self.current_node = self.first_node(false);
        self.browse_only = match scope {
            Scope::Collection => None,
            Scope::Stick(name) => Some((self.current_node, self.stick_tracks(name, root))),
        };
        self.search = query.to_string();
        self.after_jump();
    }
}
