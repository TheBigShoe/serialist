//! Watcher tests. They have to hold on FSEvents (macOS), which delivers events late, in
//! folded batches, and sometimes twice for one write, so they assert sets rather than
//! sequences and counts:
//!
//! - an event that must arrive is waited for up to [`WAIT`], and returns as soon as it
//!   has, so a fast machine pays nothing for the slack;
//! - a quiet period is only asserted for a class of event the filter must suppress, in a
//!   test that makes no legitimate change of that class, so a late duplicate of a real
//!   event can never fail it.

use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError, unbounded};
use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind};

use crate::settings::ConfigPaths;
use crate::test_util::TempDir;

use super::*;

/// How long an expected event has to show up in. FSEvents can take a second or more on a
/// loaded machine; the debounce itself is 100 ms.
const WAIT: Duration = Duration::from_secs(3);

/// How long to keep listening after an expected event, to catch one that must not exist.
const GRACE: Duration = Duration::from_millis(500);

/// Time for the OS watcher to be ready before a test touches a file.
const SETTLE: Duration = Duration::from_millis(200);

use ConfigEvent::{Commands, Keymap, Settings, Themes};

struct Fixture {
    _root: TempDir,
    paths: ConfigPaths,
    rx: Receiver<ConfigEvent>,
    watcher: ConfigWatcher,
}

/// A config directory that does not exist yet, watched.
fn watched() -> Fixture {
    let root = TempDir::new("watch");
    let paths = ConfigPaths::new(root.path().join("config"));
    let (tx, rx) = unbounded();
    let watcher = ConfigWatcher::spawn(&paths, tx);
    assert!(watcher.is_active(), "the OS watcher did not start");
    std::thread::sleep(SETTLE);
    Fixture {
        _root: root,
        paths,
        rx,
        watcher,
    }
}

/// Reads events until every kind in `wanted` has arrived, failing after [`WAIT`], then
/// keeps reading for [`GRACE`]. Returns everything that arrived, as a set.
fn expect(rx: &Receiver<ConfigEvent>, wanted: &[ConfigEvent]) -> BTreeSet<ConfigEvent> {
    let mut seen = BTreeSet::new();
    let deadline = Instant::now() + WAIT;
    while !wanted.iter().all(|kind| seen.contains(kind)) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(event) => {
                seen.insert(event);
            }
            Err(_) => panic!("waited {WAIT:?} for {wanted:?}, but only saw {seen:?}"),
        }
    }
    let end = Instant::now() + GRACE;
    while let Ok(event) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        seen.insert(event);
    }
    seen
}

/// Throws away whatever has arrived so far.
fn drain(rx: &Receiver<ConfigEvent>) {
    while rx.try_recv().is_ok() {}
}

fn set(kinds: &[ConfigEvent]) -> BTreeSet<ConfigEvent> {
    kinds.iter().copied().collect()
}

// ---- Real file system events ----

#[test]
fn a_new_settings_file_is_noticed() {
    let f = watched();
    // The file does not exist when the watcher starts.
    assert!(!f.paths.settings.exists());
    fs::write(&f.paths.settings, "{ \"buffer_font_size\": 13 }").unwrap();
    // Nothing but settings: the directories the watcher made at startup stay silent.
    assert_eq!(expect(&f.rx, &[Settings]), set(&[Settings]));
}

#[test]
fn creating_the_watcher_makes_no_event() {
    let root = TempDir::new("watch-quiet");
    let paths = ConfigPaths::new(root.path().join("a").join("b").join("serialist"));
    let (tx, rx) = unbounded();
    let watcher = ConfigWatcher::spawn(&paths, tx);
    assert!(watcher.is_active());
    // The watcher created both directories itself.
    assert!(paths.dir.is_dir());
    assert!(paths.themes.is_dir());
    assert!(paths.commands_dir().is_dir());
    // Long enough for FSEvents to have reported them, folded into a later batch.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn editing_an_existing_settings_file_is_noticed() {
    let root = TempDir::new("watch-edit");
    let paths = ConfigPaths::new(root.path());
    fs::write(&paths.settings, "{}").unwrap();
    let (tx, rx) = unbounded();
    let _watcher = ConfigWatcher::spawn(&paths, tx);
    std::thread::sleep(SETTLE);
    drain(&rx);

    fs::write(&paths.settings, "{ \"local_echo\": true }").unwrap();
    assert_eq!(expect(&rx, &[Settings]), set(&[Settings]));
}

#[test]
fn a_file_saved_by_rename_is_noticed() {
    let f = watched();
    // Editors write a temp file and rename it over the target.
    let temp = f.paths.dir.join(".settings.json.tmp");
    fs::write(&temp, "{}").unwrap();
    fs::rename(&temp, &f.paths.settings).unwrap();
    assert_eq!(expect(&f.rx, &[Settings]), set(&[Settings]));
}

#[test]
fn deleting_a_settings_file_is_noticed() {
    let root = TempDir::new("watch-delete");
    let paths = ConfigPaths::new(root.path());
    fs::write(&paths.settings, "{}").unwrap();
    let (tx, rx) = unbounded();
    let _watcher = ConfigWatcher::spawn(&paths, tx);
    std::thread::sleep(SETTLE);
    drain(&rx);

    fs::remove_file(&paths.settings).unwrap();
    assert_eq!(expect(&rx, &[Settings]), set(&[Settings]));
}

#[test]
fn a_new_keymap_file_is_noticed() {
    let f = watched();
    fs::write(&f.paths.keymap, "[]").unwrap();
    assert_eq!(expect(&f.rx, &[Keymap]), set(&[Keymap]));
}

#[test]
fn a_theme_file_is_noticed() {
    let f = watched();
    // The themes folder was made by the watcher and is already watched.
    fs::write(f.paths.themes.join("mine.json"), "{}").unwrap();
    assert_eq!(expect(&f.rx, &[Themes]), set(&[Themes]));
}

#[test]
fn a_commands_file_is_noticed() {
    let f = watched();
    // The commands folder was made by the watcher and is already watched.
    assert!(f.paths.commands_dir().is_dir());
    fs::write(f.paths.commands_dir().join("mine.json"), "{}").unwrap();
    assert_eq!(expect(&f.rx, &[Commands]), set(&[Commands]));
}

#[test]
fn a_collection_saved_by_the_store_is_noticed_once_it_is_whole() {
    let f = watched();
    let mut store = crate::CommandStore::load(&f.paths);
    store.create_collection("Mine").unwrap();
    store
        .add_command("Mine", "G", crate::Command::text("c", "AT"))
        .unwrap();
    // Written through a temp file and a rename: only the finished file is an event.
    store.save_collection("Mine").unwrap();
    assert_eq!(expect(&f.rx, &[Commands]), set(&[Commands]));
}

#[test]
fn the_project_commands_file_is_watched_too() {
    let root = TempDir::new("watch-project-commands");
    let project_file = root.write("repo/.serialist/commands.json", "{}");
    let paths =
        ConfigPaths::new(root.path().join("config")).with_project_from(&root.path().join("repo"));
    assert_eq!(
        paths.project_commands.as_deref(),
        Some(project_file.as_path())
    );
    let (tx, rx) = unbounded();
    let _watcher = ConfigWatcher::spawn(&paths, tx);
    std::thread::sleep(SETTLE);
    drain(&rx);

    fs::write(&project_file, "{ \"name\": \"Mine\" }").unwrap();
    assert_eq!(expect(&rx, &[Commands]), set(&[Commands]));
}

#[test]
fn a_theme_file_in_a_subfolder_is_noticed() {
    let f = watched();
    let pack = f.paths.themes.join("pack");
    fs::create_dir(&pack).unwrap();
    // Let the recursive watch pick the new folder up before the file lands in it.
    std::thread::sleep(SETTLE);
    fs::write(pack.join("dark.json"), "{}").unwrap();
    assert_eq!(expect(&f.rx, &[Themes]), set(&[Themes]));
}

#[test]
fn files_that_are_not_config_and_directories_are_ignored() {
    let f = watched();
    let themes = &f.paths.themes;
    let dir = &f.paths.dir;
    // None of these may produce an event.
    let commands = f.paths.commands_dir();
    fs::write(commands.join("notes.txt"), "hi").unwrap();
    fs::create_dir(commands.join("sub")).unwrap();
    fs::create_dir(commands.join("looks-like-a-collection.json")).unwrap();
    fs::write(commands.join(".mine.json.1-0.tmp"), "x").unwrap();
    fs::write(themes.join("notes.txt"), "hi").unwrap();
    fs::write(themes.join("readme.md"), "hi").unwrap();
    fs::create_dir(themes.join("pack")).unwrap();
    fs::create_dir(themes.join("looks-like-a-theme.json")).unwrap();
    fs::write(dir.join("other.json"), "{}").unwrap();
    fs::write(dir.join("settings.json.swp"), "x").unwrap();
    fs::write(dir.join(".keymap.json.tmp"), "x").unwrap();
    fs::create_dir(dir.join("settings.json.d")).unwrap();
    // The one real change, last, so that by the time it is reported the rest would have
    // been. No settings or theme file is written here, so any such event is a leak.
    fs::write(&f.paths.keymap, "[]").unwrap();
    assert_eq!(expect(&f.rx, &[Keymap]), set(&[Keymap]));
}

#[test]
fn changes_to_several_files_are_all_reported() {
    let f = watched();
    for round in 0..3 {
        fs::write(
            &f.paths.settings,
            format!("{{ \"default_baud\": {} }}", 9600 + round),
        )
        .unwrap();
        fs::write(&f.paths.keymap, "[]").unwrap();
        fs::write(
            f.paths.themes.join("t.json"),
            format!("{{ \"round\": {round} }}"),
        )
        .unwrap();
        fs::write(
            f.paths.commands_dir().join("c.json"),
            format!("{{ \"name\": \"round {round}\" }}"),
        )
        .unwrap();
    }
    // How many of each is up to the OS, so this checks the set.
    assert_eq!(
        expect(&f.rx, &[Settings, Keymap, Themes, Commands]),
        set(&[Settings, Keymap, Themes, Commands])
    );
}

#[test]
fn a_missing_config_directory_is_created_so_files_can_appear_later() {
    let root = TempDir::new("watch-missing");
    let paths = ConfigPaths::new(root.path().join("a").join("b").join("serialist"));
    assert!(!paths.dir.exists());
    let (tx, rx) = unbounded();
    let watcher = ConfigWatcher::spawn(&paths, tx);
    assert!(watcher.is_active());
    assert!(paths.dir.is_dir());
    assert!(paths.themes.is_dir());
    std::thread::sleep(SETTLE);
    fs::write(&paths.settings, "{}").unwrap();
    // The first event is the settings file, not the directories made at startup.
    assert_eq!(expect(&rx, &[Settings]), set(&[Settings]));
}

#[test]
fn the_project_settings_file_is_watched_too() {
    let root = TempDir::new("watch-project");
    let project_file = root.write("repo/.serialist/settings.json", "{}");
    let paths =
        ConfigPaths::new(root.path().join("config")).with_project_from(&root.path().join("repo"));
    assert_eq!(
        paths.project_settings.as_deref(),
        Some(project_file.as_path())
    );
    let (tx, rx) = unbounded();
    let _watcher = ConfigWatcher::spawn(&paths, tx);
    std::thread::sleep(SETTLE);
    drain(&rx);

    fs::write(&project_file, "{ \"local_echo\": true }").unwrap();
    assert_eq!(expect(&rx, &[Settings]), set(&[Settings]));
}

// ---- Shutdown ----

#[test]
fn dropping_the_watcher_is_synchronous() {
    let f = watched();
    let Fixture {
        _root,
        paths,
        rx,
        watcher,
    } = f;
    let started = Instant::now();
    drop(watcher);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "drop took {:?}",
        started.elapsed()
    );

    // Anything sent before the drop can still be read; after that the channel is
    // disconnected, because the forwarding thread that owned the sender has been joined.
    let end = loop {
        match rx.try_recv() {
            Ok(_) => continue,
            Err(err) => break err,
        }
    };
    assert_eq!(end, TryRecvError::Disconnected);

    // Changes made now go nowhere.
    fs::write(&paths.settings, "{}").unwrap();
    fs::write(&paths.keymap, "[]").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn a_watcher_that_cannot_start_is_inert_and_lets_go_of_the_sender() {
    let root = TempDir::new("watch-inert");
    // A directory cannot be made inside a file.
    let blocker = root.write("blocker", "not a directory");
    let paths = ConfigPaths::new(blocker.join("config"));
    let (tx, rx) = unbounded();
    let watcher = ConfigWatcher::spawn(&paths, tx);
    assert!(!watcher.is_active());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    drop(watcher);
}

#[test]
fn a_watcher_whose_receiver_is_gone_keeps_running_quietly() {
    let root = TempDir::new("watch-orphan");
    let paths = ConfigPaths::new(root.path());
    let (tx, rx) = unbounded();
    let watcher = ConfigWatcher::spawn(&paths, tx);
    drop(rx);
    std::thread::sleep(SETTLE);
    fs::write(&paths.settings, "{}").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    // The forwarder gave up when the send failed; dropping still joins cleanly.
    drop(watcher);
}

#[test]
fn the_forwarder_sends_each_kind_once_per_batch_and_stops_when_asked() {
    let (batch_tx, batch_rx) = unbounded();
    let (stop_tx, stop_rx) = unbounded();
    let (tx, rx) = unbounded();
    let alive = Arc::new(AtomicBool::new(true));
    let forwarder = {
        let alive = Arc::clone(&alive);
        std::thread::spawn(move || forward(&targets(), &batch_rx, &stop_rx, &tx, &alive))
    };

    let modify = EventKind::Modify(ModifyKind::Any);
    batch_tx
        .send(vec![
            event(modify, &["/cfg/settings.json"]),
            event(modify, &["/cfg/settings.json"]),
            event(modify, &["/cfg/keymap.json"]),
        ])
        .unwrap();
    assert_eq!(rx.recv_timeout(WAIT), Ok(Settings));
    assert_eq!(rx.recv_timeout(WAIT), Ok(Keymap));

    drop(stop_tx);
    forwarder.join().unwrap();
    // The forwarder owned the sender, so the channel closes with it and stays empty.
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn the_forwarder_sends_nothing_once_the_watcher_is_not_alive() {
    let (batch_tx, batch_rx) = unbounded();
    let (stop_tx, stop_rx) = unbounded();
    let (tx, rx) = unbounded();
    let alive = Arc::new(AtomicBool::new(false));
    let forwarder = {
        let alive = Arc::clone(&alive);
        std::thread::spawn(move || forward(&targets(), &batch_rx, &stop_rx, &tx, &alive))
    };
    batch_tx
        .send(vec![event(
            EventKind::Modify(ModifyKind::Any),
            &["/cfg/settings.json"],
        )])
        .unwrap();
    // Whichever of the batch and the stop it sees first, it must not send.
    drop(stop_tx);
    forwarder.join().unwrap();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn the_forwarder_ends_when_the_receiver_is_gone() {
    let (batch_tx, batch_rx) = unbounded();
    let (_stop_tx, stop_rx) = unbounded::<()>();
    let (tx, rx) = unbounded();
    let alive = Arc::new(AtomicBool::new(true));
    drop(rx);
    let forwarder = {
        let alive = Arc::clone(&alive);
        std::thread::spawn(move || forward(&targets(), &batch_rx, &stop_rx, &tx, &alive))
    };
    batch_tx
        .send(vec![event(
            EventKind::Modify(ModifyKind::Any),
            &["/cfg/settings.json"],
        )])
        .unwrap();
    // No stop signal is sent: a failed send is what ends it.
    forwarder.join().unwrap();
}

// ---- Classification, which needs no OS events ----

fn targets() -> Targets {
    Targets {
        settings: vec![
            PathBuf::from("/cfg/settings.json"),
            PathBuf::from("/proj/.serialist/settings.json"),
        ],
        keymap: PathBuf::from("/cfg/keymap.json"),
        themes: PathBuf::from("/cfg/themes"),
        commands: PathBuf::from("/cfg/commands"),
        project_commands: Some(PathBuf::from("/proj/.serialist/commands.json")),
    }
}

fn event(kind: EventKind, paths: &[&str]) -> DebouncedEvent {
    let mut event = Event::new(kind);
    for path in paths {
        event = event.add_path(PathBuf::from(path));
    }
    DebouncedEvent::new(event, Instant::now())
}

#[test]
fn a_batch_maps_to_distinct_events_in_a_fixed_order() {
    let modify = EventKind::Modify(ModifyKind::Any);
    let batch = [
        event(modify, &["/cfg/themes/b.json"]),
        event(modify, &["/cfg/keymap.json"]),
        event(modify, &["/cfg/settings.json"]),
        event(modify, &["/cfg/themes/a.json"]),
        event(EventKind::Create(CreateKind::File), &["/cfg/settings.json"]),
        event(modify, &["/proj/.serialist/settings.json"]),
        event(EventKind::Remove(RemoveKind::File), &["/cfg/themes/c.JSON"]),
    ];
    assert_eq!(
        targets().events(&batch),
        vec![Settings, Keymap, Themes],
        "each kind once, settings first"
    );
}

#[test]
fn theme_files_count_at_any_depth() {
    let modify = EventKind::Modify(ModifyKind::Any);
    for path in [
        "/cfg/themes/a.json",
        "/cfg/themes/pack/a.json",
        "/cfg/themes/pack/deep/er/a.JSON",
    ] {
        assert_eq!(targets().events(&[event(modify, &[path])]), vec![Themes]);
    }
}

#[test]
fn command_files_count_directly_in_the_commands_folder() {
    let modify = EventKind::Modify(ModifyKind::Any);
    for path in [
        "/cfg/commands/a.json",
        "/cfg/commands/B.JSON",
        "/proj/.serialist/commands.json",
    ] {
        assert_eq!(targets().events(&[event(modify, &[path])]), vec![Commands]);
    }
    for path in [
        "/cfg/commands",
        "/cfg/commands/sub/a.json",
        "/cfg/commands/a.txt",
        // The temp file `CommandStore::save` writes before it renames.
        "/cfg/commands/.a.json.123-0.tmp",
        "/cfg/commands.json",
        "/proj/commands.json",
        "/proj/.serialist/other.json",
    ] {
        assert!(
            targets().events(&[event(modify, &[path])]).is_empty(),
            "{path}"
        );
    }
    let batch = [
        event(modify, &["/cfg/commands/a.json"]),
        event(modify, &["/cfg/themes/a.json"]),
        event(modify, &["/cfg/keymap.json"]),
        event(modify, &["/cfg/settings.json"]),
    ];
    assert_eq!(
        targets().events(&batch),
        vec![Settings, Keymap, Themes, Commands]
    );
}

#[test]
fn unrelated_and_access_events_are_ignored() {
    let modify = EventKind::Modify(ModifyKind::Any);
    let access = EventKind::Access(AccessKind::Any);
    let batch = [
        event(modify, &["/cfg/settings.json.swp"]),
        event(modify, &["/cfg/.settings.json.tmp"]),
        event(modify, &["/cfg/notes.txt"]),
        event(modify, &["/cfg/other.json"]),
        event(modify, &["/cfg/themes/readme.md"]),
        event(modify, &["/cfg/themes/sub"]),
        event(modify, &["/other/settings.json"]),
        event(modify, &["/cfg/settings.json/child"]),
        event(access, &["/cfg/settings.json"]),
        event(access, &["/cfg/keymap.json"]),
        event(access, &["/cfg/themes/a.json"]),
    ];
    assert!(targets().events(&batch).is_empty());
    assert!(targets().events(&[]).is_empty());
}

#[test]
fn the_config_and_themes_directories_produce_no_events() {
    // What FSEvents reports, late, for the directories the watcher creates.
    let batch = [
        event(EventKind::Create(CreateKind::Folder), &["/cfg"]),
        event(EventKind::Create(CreateKind::Folder), &["/cfg/themes"]),
        event(EventKind::Create(CreateKind::Any), &["/cfg/themes"]),
        event(EventKind::Modify(ModifyKind::Any), &["/cfg/themes"]),
        event(EventKind::Modify(ModifyKind::Any), &["/cfg"]),
        event(EventKind::Remove(RemoveKind::Folder), &["/cfg/themes"]),
        event(EventKind::Create(CreateKind::Folder), &["/cfg/themes/pack"]),
        // A folder event is dropped whatever its name.
        event(
            EventKind::Create(CreateKind::Folder),
            &["/cfg/themes/x.json"],
        ),
        event(EventKind::Remove(RemoveKind::Folder), &["/cfg/keymap.json"]),
    ];
    assert!(targets().events(&batch).is_empty(), "{batch:?}");
}

#[test]
fn a_rename_counts_when_either_end_is_a_config_file() {
    let rename = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
    let batch = [event(
        rename,
        &["/cfg/.keymap.json.tmp", "/cfg/keymap.json"],
    )];
    assert_eq!(targets().events(&batch), vec![Keymap]);
    let batch = [event(
        rename,
        &["/cfg/settings.json", "/cfg/settings.json.bak"],
    )];
    assert_eq!(targets().events(&batch), vec![Settings]);
}

#[test]
fn a_directory_with_a_config_files_name_is_not_one() {
    let root = TempDir::new("classify");
    // The paths the OS would report, so the targets match them as they are.
    let paths = ConfigPaths::new(fs::canonicalize(root.path()).unwrap());
    fs::create_dir_all(paths.themes.join("pack.json")).unwrap();
    fs::create_dir(&paths.settings).unwrap();
    fs::write(&paths.keymap, "[]").unwrap();
    fs::write(paths.themes.join("real.json"), "{}").unwrap();
    let targets = Targets::new(&paths);
    let modify = EventKind::Modify(ModifyKind::Any);
    let on = |path: &Path| targets.events(&[event(modify, &[path.to_str().unwrap()])]);

    // On disk these are directories, even though the names fit.
    assert!(on(&paths.settings).is_empty());
    assert!(on(&paths.themes).is_empty());
    assert!(on(&paths.themes.join("pack.json")).is_empty());
    // The files are files.
    assert_eq!(on(&paths.keymap), vec![Keymap]);
    assert_eq!(on(&paths.themes.join("real.json")), vec![Themes]);
    // A file that has been deleted is not a directory, so its removal counts.
    assert_eq!(on(&paths.themes.join("gone.json")), vec![Themes]);
}

#[test]
fn canonical_resolves_through_missing_files() {
    let dir = TempDir::new("canonical");
    let real = fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        canonical(&dir.path().join("missing.json")),
        real.join("missing.json")
    );
    assert_eq!(canonical(dir.path()), real);
    let absent = dir.path().join("no-such-dir").join("file.json");
    assert_eq!(canonical(&absent), absent);
}
