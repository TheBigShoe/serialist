use std::fs;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind};

use crate::settings::ConfigPaths;
use crate::test_util::TempDir;

use super::*;

/// How long a change has to show up in. The debounce is 100 ms.
const DEADLINE: Duration = Duration::from_secs(2);

/// Time for the OS watcher to be ready before a test touches a file.
const SETTLE: Duration = Duration::from_millis(200);

struct Fixture {
    root: TempDir,
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
        root,
        paths,
        rx,
        watcher,
    }
}

/// Collects events until `quiet` passes with none, or `limit` elapses.
fn collect(rx: &Receiver<ConfigEvent>, first_wait: Duration, quiet: Duration) -> Vec<ConfigEvent> {
    let mut events = Vec::new();
    let mut wait = first_wait;
    while let Ok(event) = rx.recv_timeout(wait) {
        events.push(event);
        wait = quiet;
    }
    events
}

#[test]
fn a_new_settings_file_is_noticed() {
    let f = watched();
    // The file does not exist when the watcher starts.
    assert!(!f.paths.settings.exists());
    fs::write(&f.paths.settings, "{ \"buffer_font_size\": 13 }").unwrap();

    let started = Instant::now();
    assert_eq!(f.rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Settings));
    assert!(started.elapsed() < DEADLINE);
    // Only the settings changed.
    assert!(
        collect(
            &f.rx,
            Duration::from_millis(400),
            Duration::from_millis(200)
        )
        .is_empty()
    );
}

#[test]
fn editing_an_existing_settings_file_is_noticed() {
    let root = TempDir::new("watch-edit");
    let paths = ConfigPaths::new(root.path());
    fs::write(&paths.settings, "{}").unwrap();
    let (tx, rx) = unbounded();
    let _watcher = ConfigWatcher::spawn(&paths, tx);
    std::thread::sleep(SETTLE);

    fs::write(&paths.settings, "{ \"local_echo\": true }").unwrap();
    assert_eq!(rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Settings));
}

#[test]
fn a_file_saved_by_rename_is_noticed() {
    let f = watched();
    // Editors write a temp file and rename it over the target.
    let temp = f.paths.dir.join(".settings.json.tmp");
    fs::write(&temp, "{}").unwrap();
    fs::rename(&temp, &f.paths.settings).unwrap();
    let events = collect(&f.rx, DEADLINE, Duration::from_millis(300));
    assert_eq!(events, vec![ConfigEvent::Settings]);
}

#[test]
fn deleting_a_settings_file_is_noticed() {
    let f = watched();
    fs::write(&f.paths.settings, "{}").unwrap();
    assert_eq!(f.rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Settings));
    fs::remove_file(&f.paths.settings).unwrap();
    assert_eq!(f.rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Settings));
}

#[test]
fn a_new_keymap_file_is_noticed() {
    let f = watched();
    fs::write(&f.paths.keymap, "[]").unwrap();
    assert_eq!(f.rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Keymap));
    assert!(
        collect(
            &f.rx,
            Duration::from_millis(400),
            Duration::from_millis(200)
        )
        .is_empty()
    );
}

#[test]
fn a_theme_file_is_noticed_even_in_a_folder_made_later() {
    let f = watched();
    fs::write(f.paths.themes.join("mine.json"), "{}").unwrap();
    assert_eq!(f.rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Themes));
    assert!(
        collect(
            &f.rx,
            Duration::from_millis(400),
            Duration::from_millis(200)
        )
        .is_empty()
    );

    // Editing it again is another event.
    fs::write(f.paths.themes.join("mine.json"), "{ }").unwrap();
    assert_eq!(f.rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Themes));

    // Files that are not themes do not count.
    fs::write(f.paths.themes.join("notes.txt"), "hi").unwrap();
    assert!(
        collect(
            &f.rx,
            Duration::from_millis(500),
            Duration::from_millis(200)
        )
        .is_empty()
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
    assert_eq!(rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Settings));
}

#[test]
fn changes_to_several_files_arrive_once_each() {
    let f = watched();
    for round in 0..5 {
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
    }
    let events = collect(&f.rx, DEADLINE, Duration::from_millis(400));
    for kind in [
        ConfigEvent::Settings,
        ConfigEvent::Keymap,
        ConfigEvent::Themes,
    ] {
        let count = events.iter().filter(|event| **event == kind).count();
        // Fifteen writes inside a few milliseconds collapse into a batch or two.
        assert!(
            (1..=3).contains(&count),
            "{kind:?} arrived {count} times: {events:?}"
        );
    }
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

    fs::write(&project_file, "{ \"local_echo\": true }").unwrap();
    assert_eq!(rx.recv_timeout(DEADLINE), Ok(ConfigEvent::Settings));
}

#[test]
fn dropping_the_watcher_stops_events() {
    let f = watched();
    let Fixture {
        root: _root,
        paths,
        rx,
        watcher,
    } = f;
    drop(watcher);
    fs::write(&paths.settings, "{}").unwrap();
    fs::write(&paths.keymap, "[]").unwrap();
    // The channel is disconnected once the watcher's thread is gone, or silent.
    assert!(rx.recv_timeout(Duration::from_millis(600)).is_err());
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
    std::thread::sleep(Duration::from_millis(400));
    drop(watcher);
}

// ---- Classification, which needs no file system ----

fn targets() -> Targets {
    Targets {
        settings: vec![
            PathBuf::from("/cfg/settings.json"),
            PathBuf::from("/proj/.serialist/settings.json"),
        ],
        keymap: PathBuf::from("/cfg/keymap.json"),
        themes: PathBuf::from("/cfg/themes"),
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
        vec![
            ConfigEvent::Settings,
            ConfigEvent::Keymap,
            ConfigEvent::Themes
        ]
    );
}

#[test]
fn unrelated_and_access_events_are_ignored() {
    let modify = EventKind::Modify(ModifyKind::Any);
    let batch = [
        event(modify, &["/cfg/settings.json.swp"]),
        event(modify, &["/cfg/.settings.json.tmp"]),
        event(modify, &["/cfg/notes.txt"]),
        event(modify, &["/cfg/themes/readme.md"]),
        event(modify, &["/cfg/themes/sub"]),
        event(modify, &["/other/settings.json"]),
        event(
            EventKind::Access(notify::event::AccessKind::Any),
            &["/cfg/settings.json"],
        ),
        event(
            EventKind::Access(notify::event::AccessKind::Any),
            &["/cfg/keymap.json"],
        ),
    ];
    assert!(targets().events(&batch).is_empty());
    assert!(targets().events(&[]).is_empty());
}

#[test]
fn a_rename_counts_when_either_end_is_a_config_file() {
    let rename = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
    let batch = [event(
        rename,
        &["/cfg/.keymap.json.tmp", "/cfg/keymap.json"],
    )];
    assert_eq!(targets().events(&batch), vec![ConfigEvent::Keymap]);
    let batch = [event(
        rename,
        &["/cfg/settings.json", "/cfg/settings.json.bak"],
    )];
    assert_eq!(targets().events(&batch), vec![ConfigEvent::Settings]);
}

#[test]
fn the_themes_folder_itself_counts() {
    // Deleting and recreating the folder changes what themes exist.
    let batch = [event(
        EventKind::Remove(RemoveKind::Folder),
        &["/cfg/themes"],
    )];
    assert_eq!(targets().events(&batch), vec![ConfigEvent::Themes]);
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
