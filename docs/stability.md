# Compatibility at 1.0

> **Status: a proposal, not a decision.** This page lists what Serialist's users depend
> on and proposes a promise for each. None of the promises is accepted yet; each is for
> the owner to accept, change or drop before 1.0.0 is tagged. It describes the code at
> version 0.2.0 (commit `19b4434` and the doc fixes made with this page).

Version 1.0.0 promises that what a user's files, scripts, plugins and habits rely on keeps
working through every 1.x release. This page is the inventory of that surface, found in the
code rather than only in the docs. Each entry says:

- **Defined in**: the files and functions that fix the format or interface.
- **Versioned**: what marks its version today (a version field, a declared API number, or
  nothing).
- **Newer and older**: what this build does with a file written by a later build
  ("newer") and by an earlier one ("older").
- **Pinned by**: the tests that would fail if it changed, and what no test pins.
- **Proposed promise**: what 1.x would guarantee.
- **Before 1.0**: what has to change for that promise to be safe to make.

The owner's decisions are collected at the end, most consequential first.

## Summary

| Surface | Version marker today | Proposed 1.x promise, in short |
|---|---|---|
| Config directory | none | Same place on each OS, same file names |
| `settings.json` | none | Every 1.0 key and value keeps its meaning; keys are only added |
| `keymap.json`, action names, key contexts | none | Names and contexts kept; default chords may change with notice |
| Saved-command collections | none | Every 1.0 collection loads with the same meaning |
| `history.jsonl` | none | Format kept |
| `state.json` | `"version": 2` | App-private; earlier versions still read, newer ones ignored |
| Theme files | Zed schema v0.2.0 (not checked) | Every file 1.0 loads still loads; bundled theme names kept |
| Lua scripting API | none | Calls, arguments and results kept; additions only |
| Plugin folders | none | Folder name is the codec name; entry file names kept |
| Lua codec interface | none | A plugin that loads in 1.0 loads and decodes the same |
| `plugin.toml` and WIT | `api = "1"`, `wit/v1` | v1 never changes; a v2 sits beside it |
| `serialist-plugin-sdk` | the workspace version | Undecided: see the section |
| Command line | none | Flags, port ids and `--script` exit codes kept |
| Recordings and `.timing` | `serialist-timing 1` | v1 readable by every 1.x |
| Exports (text, raw, CSV, JSON) | none | Raw byte for byte; CSV and JSON column and key names kept |
| Rust crates | the workspace version | Internal: no promise |

## 1. Where files live

**Defined in.** `ConfigPaths::from_environment` and `ConfigPaths::new` in
`crates/serialist-core/src/settings/paths.rs`; `config::paths_for` in
`crates/serialist-ui/src/config.rs` puts `--config-dir` over the environment and adds the
project files.

The directory is `$HOME/.config/serialist` on macOS and Linux and `%APPDATA%\Serialist` on
Windows (`%USERPROFILE%\AppData\Roaming\Serialist` without `APPDATA`). With no home
directory the code falls back to `<temp>/serialist-config` and logs a warning.
`SERIALIST_CONFIG_DIR` overrides the platform rule (an empty value is ignored), and
`--config-dir` overrides both. Linux does not read `XDG_CONFIG_HOME`, and macOS does not
use `~/Library/Application Support`.

Inside it: `settings.json`, `keymap.json`, `themes/`, `commands/`, `scripts/`, `plugins/`
(with `plugins/examples/`, which the app writes and which never loads), `history.jsonl`
and `state.json`. A project adds the nearest `.serialist/settings.json` and
`.serialist/commands.json` found searching up from the working directory
(`ConfigPaths::project_settings_path`, `project_commands_path`).

**Versioned.** No. The directory holds no marker of the version that wrote it.

**Newer and older.** Not applicable: every build looks in the same place.

**Pinned by.** `config_dir_follows_the_platform`,
`the_environment_variable_overrides_every_platform`,
`with_no_home_the_config_falls_back_to_the_temp_dir`,
`default_for_platform_names_the_expected_files`,
`project_settings_are_found_by_searching_upward`,
`project_commands_are_found_by_searching_upward` and
`commands_and_history_live_in_the_config_directory` (all in
`crates/serialist-core/src/settings/tests.rs`); `scripts_live_in_the_config_directory`
and `example_plugins_are_copied_beside_the_plugins_not_into_them` in `paths.rs`. No test
covers `config::paths_for`, so `--config-dir` winning over `SERIALIST_CONFIG_DIR` is not
pinned.

**Proposed promise.** Every 1.x build reads its configuration from the same directory on
each OS, honors `--config-dir` and `SERIALIST_CONFIG_DIR` the same way, and keeps every file
and folder name listed here with its meaning. New files may be added. Nothing moves unless
the old location is still read.

**Before 1.0.** Decide whether macOS stays on `~/.config/serialist` and whether Linux
should honor `XDG_CONFIG_HOME`; either change after 1.0 needs a migration. Add a test for
`paths_for`. The `--help` text for `--config-dir` (`USAGE` in `crates/serialist/src/cli.rs`)
names `settings.json`, `keymap.json`, `themes/`, `commands/`, `scripts/` and
`history.jsonl` but not `plugins/` or `state.json`. A user plugin folder named `examples`
collides with `plugins/examples/`; the name is reserved in practice and could be reserved
in the docs.

## 2. `settings.json`

**Defined in.** The types are `Settings`, `DisplaySettings`, `TerminalSettings`,
`ReplaySettings` and `ThemeSelection` in `crates/serialist-core/src/settings/types.rs`,
`DeviceProfile`, `DeviceMatch` and `UsbId` in `profile.rs`, `InlineSettings` in
`inline.rs`, the font types in `font.rs`, and the scalar validators in `de.rs`. The
defaults are `crates/serialist-core/assets/default_settings.jsonc`. Loading and layering
are `load_settings_from_layers` in `load.rs`. The app's reaction to a bad file is
`Config::reload_settings` in `crates/serialist-ui/src/config.rs`; the Settings screen
writes through `SettingsEditor` (`settings/edit.rs`) from `write_setting` in
`crates/serialist-ui/src/settings_io.rs`.

The keys are: `buffer_font_family`, `buffer_font_size`, `buffer_font_weight`,
`buffer_font_features`, `buffer_line_height`, `buffer_font_fallbacks`, `ui_font_family`,
`ui_font_size`, `ui_font_weight`, `ui_font_features`, `ui_font_fallbacks`, `terminal`
(`font_family`, `font_size`, `font_weight`, `font_features`, `font_fallbacks`,
`line_height`, `emulation`, `cursor_blink`), `theme` (a name, or `mode`, `light`, `dark`),
`display` (`timestamps`, `timestamp_format`, `view`, `hex_bytes_per_row`,
`show_control_chars`, `wrap`, `decoded_inline`, `hide_framed_bytes`),
`scrollback_budget_bytes`, `default_baud`, `line_ending`, `local_echo`, `restore_session`,
`replay` (`speed`, `end`), `inline` (`backspace`, `escape_chord`, `paste_chunk_bytes`,
`paste_chunk_delay_ms`) and `devices`. A device profile has `name`, `match` (`vid`, `pid`,
`product`, `manufacturer`, `serial_number`, `path`), `baud`, `data_bits`, `parity`,
`stop_bits`, `flow_control`, `plugin`, `eol`, `on_connect` and `emulation`; `match` is
required (the field has no serde default), the keys inside it are not. The font and theme
keys and the `terminal.font_*` and `terminal.line_height` keys are Zed's names and take
Zed's value forms.

Value forms that the loader accepts beyond the documented ones, and so become part of the
surface once users write them: `parity` also takes `"n"`, `"o"`, `"e"`, `"m"`, `"s"` and
their capitals; `flow_control` takes `"rtscts"` and `"xonxoff"`; `inline.backspace` takes
`"delete"`, `"backspace"`, `"0x7f"`, `"127"`, `"0x08"`, `"0x8"`, `"8"` and the numbers 127
and 8; `vid` and `pid` take a hex string without `0x` (`"0e8d"`). `data_bits` and
`stop_bits` take numbers or `"five"` to `"eight"` and `"one"`, `"two"`, as the defaults
file documents. The Settings screen writes numbers for `data_bits` and `stop_bits` and
names for the rest (`ProfileEditor::to_json` in
`crates/serialist-ui/src/settings_view/devices.rs`), and leaves out a profile key equal to
the global value unless the profile already had it.

Layering: the bundled defaults, then the user file, then the project file. Objects merge
key by key; arrays and scalars replace, so a project file's `devices` replaces the user's
whole list. `null` unsets an optional key. The template written by Open settings.json
(`settings_template`) is every default commented out, so a typical user file sets a few
keys and follows every default.

**Versioned.** No version field.

**Newer and older.** A key this build does not know is ignored and reported as a
`SettingsWarning` ("unknown setting `display.wrapp` is ignored"), shown in the status line.
A value it does not accept (a new enum value, a number out of range, a wrong type) fails
the whole layer with `SettingsError::Invalid` naming file, line and column. The app then
keeps the last settings that loaded, which at startup are the bundled defaults, and shows
"Settings not applied: …". A headless `--script` run uses the defaults and logs a warning
(`configured_options` in `crates/serialist/src/headless.rs`). So a file written for a later
version that adds a key loads in an earlier build with a warning, but one that uses a new
value for an existing key (a fifth `line_ending`, a third `emulation`) loses every user
setting in the earlier build. An older file loads in a newer build as long as no key or
value it uses was removed.

**Pinned by.** In `crates/serialist-core/src/settings/tests.rs`:
`bundled_defaults_are_complete_and_as_documented`,
`a_partial_document_deserializes_with_bundled_defaults`,
`comments_and_trailing_commas_are_accepted`, `a_non_object_root_is_rejected`,
`layering_is_defaults_then_user_then_project`, `unknown_keys_are_warnings_not_errors`,
`out_of_range_values_are_errors_with_positions`,
`sizes_and_heights_that_do_not_fit_an_f32_are_errors`, `null_unsets_optional_keys`,
`a_theme_string_replaces_a_theme_object_and_back`,
`line_height_has_three_forms_and_zeds_custom_object`,
`usb_ids_accept_hex_strings_and_integers`, `line_settings_accept_numbers_and_names`,
`device_matching_rules`, `the_first_matching_profile_wins`,
`replay_settings_read_every_speed_spelling_and_keep_what_the_file_leaves_out`,
`an_unknown_replay_key_is_reported_and_ignored`,
`uncommenting_a_template_line_overrides_that_key`. In `settings/inline_tests.rs`:
`defaults_are_the_documented_values`, `every_key_is_read`,
`backspace_takes_names_and_byte_values`, `bad_values_are_errors_with_positions`. The app's
side: `a_broken_settings_file_keeps_the_last_good_settings_and_shows_why`
(`crates/serialist-ui/src/config_tests.rs`) and
`a_bad_value_is_refused_beside_its_control_and_the_file_is_untouched`
(`settings_tests.rs`); the comment-preserving editor by
`crates/serialist-core/tests/settings_edit.rs`.

Not pinned: the `parity` letters and the `"rtscts"` and `"xonxoff"` spellings; no test
keeps a settings file written by 0.1.0 or 0.2.0 as a fixture; nothing tests that a
project file's `devices` replaces the user's list rather than adding to it.

**Proposed promise.** Every key and value a 1.0 settings file can hold, the extra
spellings above included, keeps its meaning in every 1.x. New keys may be added; a key may
be deprecated but is still read. An unknown key is a warning, never an error. A changed
default is named in the changelog.

**Before 1.0.** Decide how a later 1.x value meets an earlier 1.x build. Either change the
loader so that a value it cannot read rejects that key only (warning, default kept) rather
than the whole file, or promise that 1.x never adds a value to an existing key and gives
new behavior a new key instead. Decide whether the extra spellings are promised or removed
now. Decide whether default values are part of the promise (see the decisions list); since
the template comments every key out, a changed default reaches every user. Add a fixture
of a full 0.2.0 settings file to the tests.

## 3. `keymap.json`, action names and key contexts

**Defined in.** Parsing: `Keymap::parse` and `load_keymap` in
`crates/serialist-core/src/keymap.rs`. The defaults:
`crates/serialist-core/assets/keymaps/{macos,linux,windows}.json` (the Linux and Windows
files are identical). Binding: `keymap::gpui_bindings` and `keymap::apply` in
`crates/serialist-ui/src/keymap.rs`. Action names and key contexts:
`crates/serialist-ui/src/actions.rs`.

The format is Zed's: a JSON array (comments allowed) of sections, each with an optional
`context`, an optional `use_key_equivalents` and `bindings` mapping keystrokes to an
action name, `[name, arguments]`, or `null` to unbind. The user file is applied after the
defaults. A saved command's `keybinding` becomes a `commands::Send` binding in
`Workspace`, applied after both.

The actions, by namespace:

- `serialist::`: `Quit`, `OpenSettingsUi`, `OpenSettings`, `OpenKeymap`,
  `OpenThemesFolder`, `ReloadConfig`.
- `terminal::`: `Clear`, `JumpToBottom`, `Pause`, `Export`, `ToggleRecord`,
  `ToggleInline`, `Paste`, `ToggleEmulation`, `Copy`, `SelectAll`, `Search`,
  `DismissSearch`, `SearchNext`, `SearchPrevious`, `ToggleWrap`, `CycleTimestamps`,
  `ToggleHexView`, `ToggleFrameStats`, `PageUp`, `PageDown`, `ScrollToTop`.
- `serial::`: `Connect`, `Disconnect`, `ConnectTcp`, `OpenCapture`.
- `devices::`: `SelectNext`, `SelectPrevious`.
- `commands::`: `SelectNext`, `SelectPrevious`, `SendSelected`, `EditSelected`,
  `NewCommand`, `NewCollection`, and `Send` with `{ "collection", "group", "name" }`.
- `command_palette::`: `Toggle`, `SelectNext`, `SelectPrevious`.
- `tabs::`: `NewTab`, `CloseTab`, `NextTab`, `PreviousTab`, `ActivateTab1` to
  `ActivateTab9`.
- `scripts::`: `RunInline`, `Stop`, `ClearConsole`, `OpenScriptsFolder`, and `Run` with
  `{ "path" }`.
- `plugins::`: `OpenPluginsFolder`, and `InstallExamplePlugin` with `{ "name" }`.
- `compose::`: `HistoryPrevious`, `HistoryNext`, `CycleLineEnding`, `SaveAsCommand`.

The three actions with arguments reject unknown argument keys (`deny_unknown_fields`).
The key contexts the views set are `Workspace`, `DevicesPanel`, `SessionView`,
`ComposeBar`, `CommandsPanel`, `Terminal`, `TerminalInline`, `TerminalSearch`,
`ScriptConsole`, `CommandPalette` and `SettingsView`; the bundled keymaps also use
`CommandsPanel > Input`, `ComposeBar > Input` and `CommandPalette > Input`, where `Input`
is the text field's own context, which no Serialist view sets. `cx.build_action` resolves
any registered action, so gpui-kit's own actions can be bound from `keymap.json` too,
though Serialist does not own their names.

**Versioned.** No.

**Newer and older.** A syntax error or a wrong shape fails the whole file; the app keeps
the last bindings that loaded and says "Keymap not applied: …". An entry naming an action
this build does not have, arguments the action rejects, or a keystroke or context that does
not parse is skipped and reported, and the rest bind. So a later keymap that names a new
action, or gives `scripts::Run` a new argument, degrades one entry at a time in an earlier
build. Unknown keys in a section are ignored.

**Pinned by.** In `crates/serialist-core/src/keymap/tests.rs`:
`string_array_and_null_actions`, `action_arguments_can_be_any_json`,
`sections_without_a_context_bind_everywhere`, `use_key_equivalents_is_carried_per_section`,
`comments_trailing_commas_and_empty_files`, `errors_carry_file_line_and_column`,
`a_user_keymap_is_appended_after_the_defaults`, `load_keymap_handles_missing_and_broken_files`,
`the_scan_finds_the_ui_actions`, `every_platform_default_loads_and_names_only_real_actions`,
`the_defaults_reproduce_the_apps_bindings`. In the app:
`test_chords_match_the_bundled_keymap` (`actions.rs`, 24 default chords on the platform
the tests run on),
`a_keymap_file_rebinds_clear_and_reloads_without_duplicates`,
`keymap_entries_that_cannot_bind_are_reported_and_the_rest_bind` (`config_tests.rs`),
`a_key_binding_in_the_user_keymap_runs_a_script` (`script_tests.rs`),
`command_keybindings_send_and_conflicts_are_reported` (`commands_tests.rs`),
`rebinding_a_key_writes_the_user_keymap_and_the_new_chord_works` (`settings_tests.rs`).

Not pinned: the complete list of action names and contexts. `the_scan_finds_the_ui_actions`
checks seven names, and `every_platform_default_loads_and_names_only_real_actions` checks
that the bundled keymaps name real actions, so a rename that also edits the bundled
keymaps passes every test while breaking users' files. No test binds
`plugins::InstallExamplePlugin` from a keymap file.

**Proposed promise.** Every action name, key context and argument shape listed here
works in every 1.x. A renamed action keeps answering to its old name. Default chords may
change in a minor release, named in the changelog; a binding in the user's `keymap.json`
always wins. gpui-kit's actions are not covered.

**Before 1.0.** Add a test that pins the full list of action names and contexts (a
snapshot of what `declared_actions` and `declared_contexts` scan). Decide whether
diagnostic actions such as `terminal::ToggleFrameStats` belong in the promised set. Decide
whether default chords are promised (see the decisions list).

## 4. Saved-command collections

**Defined in.** `crates/serialist-core/src/commands/`: `model.rs` (`Command`, `Payload`,
`Expect`, `Param`, `ParamKind`), `file.rs` (`CommandCollection::parse`, `to_json`, the
known-key lists), `payload.rs` (escapes and placeholders) and `store.rs` (`CommandStore`).
The bundled read-only collection is `crates/serialist-core/assets/commands/examples.json`.

A collection is `commands/<file>.json` (one per file) or a project's
`.serialist/commands.json`: `{ "name", "groups": [ { "name", "commands": [ … ] } ] }`. A
command has `name`, `payload` and optionally `description`, `eol`, `expect`, `keybinding`
and `params`. The payload is exactly one of `{ "text" }`, `{ "hex" }`,
`{ "codec", "fields" }` or `{ "script" }`. `expect` is `{ "pattern", "timeout_ms", "frame" }`
(`timeout_ms` defaults to 1000). A param is `{ "name", "label", "default", "kind" }` with
`kind` one of `text`, `int`, `hex16`. Text payloads take the escapes
`\r \n \t \0 \\ \{ \} \xNN`; both text and hex payloads take `{{name}}` placeholders, and hex
payloads take `{{name:be}}`. A codec payload's `fields` may hold `command` to pick the
codec's command. `keybinding` and `commands.send` name a command as
`collection/group/name`.

**Versioned.** No.

**Newer and older.** A key outside the known lists is reported as a warning and ignored.
A file that does not parse, or whose values do not fit the model (a `kind` this build does
not know, a payload with none of the four forms it knows), is skipped whole with a warning,
and the other collections load. When the Commands panel saves an edit it rewrites the whole
file as plain JSON from the model, which drops comments and unknown keys
(`CommandCollection::to_json`). So a collection that a later build wrote with a new key
loses that key the first time an earlier build edits it, and one that uses a new payload
form or param kind does not load in the earlier build at all.

**Pinned by.** In `crates/serialist-core/src/commands/tests.rs`:
`the_plans_example_file_parses_field_by_field`, `defaults_for_omitted_keys`,
`an_expect_can_be_a_frame_predicate`, `a_script_payload_runs_a_script_and_has_no_bytes`,
`a_payload_needs_exactly_one_form`, `a_wrong_shape_is_an_error_not_a_panic`,
`unknown_keys_and_problems_become_warnings`, `a_bad_file_is_a_warning_and_the_rest_still_load`,
`saving_drops_comments_and_unknown_keys`, `the_bundled_examples_load_clean_and_encode`,
and the payload tests (`text_escapes`, `hex_payloads_expand_hex16_little_endian_by_default`,
`hex_payloads_expand_int_as_one_byte`, `hex_and_codec_payloads_send_no_line_ending_unless_told_to`
and their neighbours). The codec payload and frame predicate in the app:
`a_codec_command_without_its_plugin_is_not_sent_and_says_what_to_install`
(`plugin_tests.rs`) and the Decoded panel tests.

Not pinned: no collection written by 0.1.0 or 0.2.0 is kept as a fixture other than the
bundled examples, which change with the app.

**Proposed promise.** Every collection that loads in 1.0 loads with the same meaning in
every 1.x: the keys, the four payload forms, the escapes, the placeholder syntax, the
param kinds and the `expect` forms. New keys, payload forms and param kinds may be added.

**Before 1.0.** Decide whether the writer keeps keys it does not know (carry them through
from the parsed JSON) so an edit in an earlier build does not destroy a later build's data.
Consider skipping one command with a warning, rather than the whole file, when a command
uses a payload form or param kind this build does not know.

## 5. `history.jsonl`

**Defined in.** `History` in `crates/serialist-core/src/history.rs`. One JSON string per
line, oldest first, at most `MAX_ENTRIES` (1000). A line that is not a JSON string is
skipped with a warning and dropped at the next save. Written whole through a temporary
file and a rename.

**Versioned.** No.

**Pinned by.** `the_file_holds_one_json_string_per_line_oldest_first`,
`a_corrupt_line_is_skipped_with_a_warning`, `a_windows_file_and_bad_utf8_still_load`,
`only_the_newest_thousand_are_kept` (`crates/serialist-core/src/history/tests.rs`) and
`compose_history_persists_across_workspaces` (`commands_tests.rs`).

**Proposed promise.** The format stays one JSON string per line, oldest first, in every
1.x. The number of entries kept is not promised.

**Before 1.0.** Nothing.

## 6. `state.json`

**Defined in.** `SessionState`, `SavedTab`, `STATE_VERSION` (2) and `OLDEST_VERSION` (1) in
`crates/serialist-ui/src/session_state.rs`; `SavedDocks` in `docks.rs`. The file holds
`version`, `active`, `tabs` (each `port`, `serial`, `codec`, `mode`, `emulation`) and
`docks`. `serial` is `SerialConfig`'s own serde form (`"data_bits": "eight"`,
`"stop_bits": "one"`), not the settings file's numbers. It is written whole at quit
through `state.json.tmp` and a rename.

**Versioned.** Yes: `"version": 2`. Version 2 added `emulation`; version 1 still loads.

**Newer and older.** A file of version 1 or 2 loads; missing optional keys take defaults.
A file of a version outside 1 to 2, or one that does not parse (including a version-2
file with a `mode` or `emulation` value this build does not know), is ignored with a log
line, and the app starts with no tabs. At the next quit the build overwrites it with its
own version, so a later build's tabs are lost after running an earlier one. A restored
`tcp:` id that does not parse leaves its tab waiting with a Connect button; a `replay:` tab
always waits, and an option this build does not know makes its Connect fail (section 14).

**Pinned by.** `state_round_trips_through_the_file`, `a_bad_or_foreign_file_is_ignored`,
`a_version_1_file_still_loads_without_an_emulation`, `saving_needs_the_directory`
(`session_state.rs`); `the_open_tabs_come_back_at_the_next_start`,
`a_version_1_state_file_reopens_its_tabs_with_the_emulation_the_settings_name`,
`restore_session_off_starts_with_no_tabs` (`tab_tests.rs`);
`restored_tabs_of_every_kind_each_do_their_own_thing`,
`a_restored_tcp_id_that_does_not_parse_waits_for_connect` (`transport_tests.rs`).

**Proposed promise.** `state.json` is the app's own file, not an interface: nobody is
expected to edit it or read it. Every 1.x reads the state files of every earlier 1.x (and
versions 1 and 2), bumps `version` whenever the format changes, and ignores a newer file
rather than misreading it.

**Before 1.0.** Decide whether an earlier build should overwrite a later build's file at
quit; leaving a newer file alone (or setting it aside) costs a few lines in
`SessionState::save`'s caller.

## 7. Theme files

**Defined in.** `ThemeFamily::parse` in `crates/serialist-core/src/theme/family.rs`;
`ThemeRegistry` in `registry.rs`; the keys the app reads, `USED_STYLE_KEYS` and
`TERMINAL_ANSI_KEYS`, in `keys.rs`. The bundled files are in
`crates/serialist-core/assets/themes/`. A file is Zed's theme family format,
`{ "name", "author", "themes": [ { "name", "appearance", "style" } ] }`, read from every
`*.json` in `themes/` in file name order. The bundled theme names are Serialist Dark,
Serialist Light, Serialist Ember, Serialist Phosphor, Serialist Paper, Serialist Contrast,
Fadetouched and Fadetouched Blur; `theme` in settings names them. `fadetouched.json` is a
third party's file kept byte for byte.

**Versioned.** The bundled files carry `"$schema": "https://zed.dev/schema/themes/v0.2.0.json"`,
but the parser does not read `$schema`. Nothing else marks a version.

**Newer and older.** The parser is lenient by design. A file needs a `name` and a `themes`
list or it is skipped with a warning. A theme entry without a name, an appearance of
`"light"` or `"dark"`, or a `style` object is skipped with a warning; a style value that is
null or not a color is skipped with a warning; unknown keys are kept and ignored. A theme
that leaves out a key the app reads falls back for that key (`Theme::color_or`). A theme
the settings name and no file provides falls back to the bundled one, with a warning. A
user theme with a bundled theme's name replaces it.

**Pinned by.** In `crates/serialist-core/src/theme/tests.rs`:
`the_fixture_reads_every_hex_form_and_the_nested_tables`,
`color_or_falls_back_only_for_missing_keys`, `bad_style_values_are_skipped_with_a_warning`,
`unusable_theme_entries_are_skipped_but_the_family_loads`,
`theme_files_may_have_comments_and_trailing_commas`, `broken_theme_files_are_errors_with_positions`,
`the_bundled_themes_define_every_key_the_app_reads`, `user_theme_files_load_and_override_by_name`,
`resolve_falls_back_to_the_bundled_theme_and_warns`,
`default_settings_resolve_to_the_bundled_themes`. In the app:
`a_zed_theme_colors_the_terminal_and_the_chrome` and `a_missing_theme_falls_back_and_says_so`
(`config_tests.rs`).

Not pinned: no theme file from Zed's own repository is a fixture
(`crates/serialist-core/tests/fixtures/theme_family.json` is written for the tests), so
"Zed theme files load unchanged" rests on the lenient parser.

**Proposed promise.** Every theme file that loads in 1.0 loads in every 1.x and colors
the keys it sets the same way. The bundled theme names stay. Following later Zed theme
schemas is best effort, not promised.

**Before 1.0.** Add a real Zed theme file (one whose license allows it) as a fixture.

## 8. The Lua scripting API

**Defined in.** `crates/serialist-script/src/`: the globals in `api.rs` (`print`, `log`,
`sleep`, `ui`, `commands`, `hex`, `bytes`, `serial`, `require`, `dofile`), the port object
in `port.rs`, the sandbox in `vm.rs`, limits and outcomes in `host.rs`, the headless host in
`headless.rs`. The crate docs in `lib.rs` hold the API table, which `docs/scripting.md`
copies and names as the source. The language is Lua 5.4 (mlua with `lua54`, vendored).

Beyond the table: `port:read`, `read_line` and `expect` fail with the strings `"timeout"`
and `"closed"`; a stopped run raises `script stopped`; `serial.ports()` entries have `kind`
`"usb"`, `"bluetooth"`, `"pci"`, `"virtual"` or `"unknown"`. In the window, `serial.open`
returns `nil` and a message; headless, `serial.open{ match = … }` does and
`serial.ports()` returns an empty list. A script is started from the Script console, the
Scripts menu, `scripts::Run`, a saved command's `{ "script" }` payload, a device profile's
`on_connect`, or `--script`.

`codecs.<name>.encode`, `timer.after` and `timer.every`, and `port:on_frame` were added
alongside this page. `docs/scripting.md` describes them, and they are part of the proposed
1.0 surface with the rest of the table.

**Versioned.** No. A script cannot ask which version of the API it runs against.

**Newer and older.** A script that calls a function this build lacks fails with Lua's
"attempt to call a nil value", which ends the run with exit status 1 headless.

**Pinned by.** `crates/serialist-script/tests/host.rs` (`print_and_log_arrive_as_output_in_order`,
`errors_carry_a_traceback_with_the_script_name_and_line`, `the_memory_cap_is_an_error_not_an_abort`,
`stop_ends_a_busy_loop_within_a_second`, `ui_prompt_and_notify_round_trip`,
`commands_send_goes_through_the_sender`, `the_sandbox_has_no_os_io_or_escape_hatches`,
`a_symlink_out_of_the_scripts_directory_is_refused`, `hex_and_bytes_helpers`,
`waiting_inside_a_coroutine_the_script_made_fails_clearly`,
`a_script_without_a_session_is_told_so`) and `tests/port.rs`
(`version_probe_against_the_at_modem`,
`an_expect_that_never_matches_times_out_with_nil_and_elapsed`,
`expect_captures_and_reads_the_stream_in_order`, `read_line_and_read_with_timeouts`,
`on_line_runs_while_the_script_sleeps_and_stops_after_cancel`,
`on_line_callbacks_may_wait_and_their_errors_end_the_run`,
`a_disconnect_ends_waits_with_closed`, `set_drives_control_lines_and_baud`,
`serial_open_and_ports_through_the_headless_opener`). The triggers in the app:
`crates/serialist-ui/src/script_tests.rs`.

Not pinned: the text of `port:description()` and of error messages other than
`"timeout"`, `"closed"` and `script stopped`, which are for people.

**Proposed promise.** Every call in the API table keeps its name, its arguments and the
shape of what it returns, including the `"timeout"` and `"closed"` results, in every 1.x.
New globals, functions and optional arguments may be added. The sandbox does not take away
a library scripts have. Lua stays 5.4. Message texts and `port:description()` are not
promised.

**Before 1.0.** Give scripts a value to check (for example `serialist.version`, or an API
number) so a script written for a later 1.x can fail clearly on an earlier one. Decide
whether the three additions in progress land before 1.0. Decide whether the headless
output forms (`print` tab-joined, `log.*` as `[info] …`) are promised, since tools parse
stdout.

## 9. Plugin folders

**Defined in.** `find_plugins` and `load_plugins` in
`crates/serialist-plugins/src/dir.rs`; the app's registry in
`crates/serialist-ui/src/codecs.rs`; `ConfigPaths::install_example_plugin` and
`ensure_example_plugins` in `paths.rs`; the examples in
`crates/serialist-plugins/src/examples.rs`.

A plugin is a folder directly under `plugins/`. A folder with `plugin.lua` is a Lua plugin
even if it also has `plugin.wasm`; otherwise `plugin.wasm` with `plugin.toml` is a
WebAssembly plugin. The app registers a plugin under its folder's name, not the name
`describe` returns (`load_plugins`, the library function, uses the described name). That
folder name is what a device profile's `plugin`, a saved command's `codec`, a restored
tab's `codec` and the codec menu use. The examples install as `airoha-race` (Lua) and, in a
build with the `wasm` feature, `airoha-race-wasm`. The host makes frames of kind
`plugin_error` (field `error`) and `codec_error` (`CODEC_ERROR_KIND` in
`crates/serialist-core/src/frames.rs`) for plugins that fail.

**Versioned.** No.

**Pinned by.** `plugins_are_found_by_folder_and_webassembly_is_recognised` and
`the_example_plugins_load_once_installed` (`crates/serialist-plugins/tests/lua_adapter.rs`);
`a_fresh_configuration_has_no_codec_and_shows_nothing_about_codecs`,
`installing_the_example_from_the_palette_brings_its_codec_in_through_the_watcher`,
`a_plugin_of_ones_own_brings_the_codec_menu_which_offers_the_examples` and
`a_profile_naming_a_missing_plugin_connects_without_a_codec_and_offers_to_install_it`
(`crates/serialist-ui/src/plugin_tests.rs`).

**Proposed promise.** The folder layout, the entry file names, the rule that the folder
name is the codec's name, the example names and the `plugin_error` and `codec_error` kinds
stay in every 1.x.

**Before 1.0.** Nothing, beyond the Lua and WebAssembly questions below.

## 10. The Lua codec interface

**Defined in.** `crates/serialist-plugins/src/lua/mod.rs` (the contract, in its module
docs), `convert.rs` (frames, fields, requests and the byte budget), `vm.rs` (the sandbox
and the `codec` table). `docs/plugins.md` restates it.

`plugin.lua` returns a table with `describe`, `decode` and `encode`. `describe()` returns
`name`, `version`, `description`, `kinds` (each `kind`, `description`, `fields`) and
`commands`; a field is `name`, `type` (`bool`, `int`, `uint`, `float`, `str`, `bytes`,
`list`), `description`, `optional`. `decode(bytes, state)` returns frames (`kind`, `pos`,
`len`, `severity`, `summary`, `fields`) and the bytes to hold back.
`encode(request)` returns bytes or `nil` and one of `codec.unknown_command`,
`codec.missing_field`, `codec.bad_field`; requests use `codec.null` and `codec.is_array`.
The sandbox has `string`, `table`, `math`, `utf8`, `coroutine`, `os.time`, `os.clock`,
`os.date`, `hex`, `bytes`, `codec`, `print` and `log`. `LuaLimits` defaults: 64 MiB of
memory, 20 000 000 instructions per call checked every 10 000, 1 MiB held back, 64 MiB of
frames per call, 1 MiB for `describe`.

**Versioned.** No. A Lua plugin has no manifest and declares no API number;
`describe().version` is the plugin's own version. (The plan's "Open questions" answer
says a plugin's `plugin.toml` declares `api = "1"`; that holds for WebAssembly plugins
only.)

**Newer and older.** A plugin that uses something this build lacks fails at load, or the
call fails and its bytes become a `plugin_error` frame; ingest never stalls.

**Pinned by.** `crates/serialist-plugins/tests/lua_adapter.rs`
(`held_back_bytes_are_prepended_and_offsets_stay_global`,
`frames_that_break_the_contract_are_reported_not_trusted`,
`frames_out_of_order_spoil_the_rest_of_the_call`,
`a_frame_of_a_kind_describe_does_not_list_is_an_error`,
`the_frame_budget_is_a_limit_like_the_others`, `the_describe_budget_is_a_limit_like_the_others`,
`encode_maps_results_and_errors`, `the_sandbox_has_no_io_os_or_debug`,
`a_plugin_that_cannot_load_says_why`, `reload_picks_up_edits_and_keeps_the_old_plugin_on_failure`,
`reset_forgets_held_bytes_and_state`) and the Rust-against-Lua tests in
`tests/conformance.rs` (`both_describe_the_same_codec`,
`rust_and_lua_agree_on_any_capture_however_it_is_cut`, `rust_and_lua_encode_any_request_alike`).

**Proposed promise.** A `plugin.lua` that loads in 1.0 loads and decodes the same in every
1.x: the three functions, the description and frame tables, the field types, the `codec`
helpers and the sandbox's libraries stay. Limits may be raised, not lowered.

**Before 1.0.** Decide whether Lua plugins declare an API (an `api` key in what
`describe()` returns, or a `plugin.toml` beside `plugin.lua`). Without one, a later
incompatible interface cannot tell an old plugin from a new one.

## 11. `plugin.toml` and the WebAssembly interface

**Defined in.** `PluginManifest` and `API_VERSION` (`"1"`) in
`crates/serialist-plugins/src/wasm/manifest.rs` and `wasm/mod.rs`; the world in
`crates/serialist-plugins/wit/v1/serialist-codec.wit` (package `serialist:codec@1.0.0`,
world `plugin`); loading and limits in `wasm/codec.rs` and `wasm/engine.rs`.

`plugin.toml` holds `name`, `version`, `api` and an optional `description`; other keys are
ignored. `name` must not be empty, `api` must be `"1"`, and `name` and `version` must match
`describe`. The component exports `describe`, `decode`, `encode` and `reset` and imports
only `log` (`ALLOWED_IMPORTS`); it gets no WASI. Limits: 64 MiB of memory, 50 ms per call,
512 KiB of wasm stack, 1 MiB held back, 64 log lines per call.

**Versioned.** Yes: `api = "1"` in the manifest and the `wit/v1` directory. The
component's own import and export names are bare (`log`, `describe`, …) and carry no
version, and the host does not check the WIT package version, so the manifest's `api` is
the only check. The WIT file states the plan for change: "A later, incompatible API gets
its own directory (`wit/v2/`) and package version, and the host keeps loading v1 plugins."

**Newer and older.** A manifest with an `api` other than `"1"` is refused with a message
naming the API this build loads. In the component model a changed record, enum or variant
(another `severity`, another `field-type`, another `json` case, a new record field)
changes the types the host links against, so v1 cannot grow at all: any change is v2.

The `wasm` feature of `serialist-plugins` is off by default, and the release packages are
built without it (`packaging/package.sh` runs `cargo build --release --locked -p
serialist`). No released build loads `plugin.wasm`; such a folder is found and reported
as needing the feature.

**Pinned by.** `a_manifest_names_the_plugin_and_pins_the_api` and
`bad_manifests_say_what_is_wrong` (`manifest.rs`); in
`crates/serialist-plugins/tests/wasm_plugin.rs`,
`the_race_plugin_loads_from_its_folder_with_its_manifest`,
`plugin_folders_load_and_bad_ones_say_why`, `components_that_are_not_plugins_are_refused_at_load`,
`the_race_plugin_and_the_manifest_in_the_example_agree`,
`the_committed_plugins_match_their_sources` (the committed builds under
`tests/fixtures` are real v1 components), and the contract tests beside them; the
Rust-against-WebAssembly tests in `tests/conformance.rs`. They run only with the feature,
in CI's own job.

**Proposed promise.** Every 1.x build that has the WebAssembly tier loads every `api = "1"`
component built against `wit/v1`, unchanged. `wit/v1` never changes; a new interface is
`wit/v2` with `api = "2"`, loaded beside v1.

**Before 1.0.** Decide whether the release packages ship the WebAssembly tier. If they do
not, this promise reaches only people who build from source, and the owner may prefer to
leave the WebAssembly tier out of the 1.0 promise.

## 12. `serialist-plugin-sdk`

**Defined in.** `crates/serialist-plugin-sdk`: the `Plugin` trait, frame and info
builders, `Request`, `hex`, `log`, `export_plugin!`, the `rt` feature, and `pub mod
bindings` (the wit-bindgen output, public "for the rare plugin that wants the raw WIT
shapes").

**Versioned.** It takes the workspace version (0.2.0), so its version moves with every app
release whether or not the SDK changed.

**Newer and older.** Not applicable until it is published.

It is not published, and cannot be as it stands: `wit_bindgen::generate!` reads
`../serialist-plugins/wit/v1`, outside the crate, so a packaged crate would not build.
`docs/plugins.md` says so ("Nothing in this repository says the SDK is published") and
tells plugin authors to use a path dependency. The public `bindings` module ties the SDK's
own semver to wit-bindgen's generated types.

**Pinned by.** Its unit tests (`nested_lists_flatten_into_items_one_list_each`,
`round_trips_and_accepts_every_form`, `parses_hex_numbers`,
`fields_read_with_the_builtin_conventions`, `unknown_fields_are_reported_alphabetically_first`)
and the reference plugin `examples/plugins/airoha-race-wasm`, held to the Rust codec by the
conformance tests.

**Proposed promise.** Either: the SDK is internal until a later release, and plugin
authors depend on the WIT, not the crate. Or: it is published, with its own version line
(1.x meaning "builds `wit/v1` plugins"), and follows semver for its Rust API.

**Before 1.0 (if published).** Copy the WIT into the crate (or generate it in a build
script from a copy), give the crate its own version, and decide whether `bindings` stays
public.

## 13. The command line

**Defined in.** `parse` and `USAGE` in `crates/serialist/src/cli.rs`; `main` and
`run_script` in `crates/serialist/src/main.rs`; `run` and `exit_code` in
`crates/serialist/src/headless.rs`; the simulated device names in
`crates/serialist/src/wiring.rs` and `serialist-sim`.

Flags: `--port <ID>` (repeatable), `--baud <N>`, `--virtual [NAME]` (repeatable; the name is
taken only when the next argument does not start with `-`), `--config-dir <DIR>`,
`--script <PATH>`, `--terminal-demo`, `-h`/`--help`, `-V`/`--version`. Every flag that takes
a value also takes `--flag=value`. `--version` prints `serialist <version>`. A bad flag
prints `serialist: <error>` and the usage to stderr and exits 2. The simulated devices are
`echo`, `echo-lines`, `at`, `firehose`, `firehose-ansi`, `race` and `menu`; an unknown
one exits 2. Environment: `SERIALIST_CONFIG_DIR` and `RUST_LOG`.

`--script` exit statuses: 0 when the main chunk returns, 1 on an error (including a port
that cannot be opened), 2 when the script was stopped or nothing ran (a bad flag, no
`--port` or more than one, an unreadable script, an unknown simulated device). Headless,
stdout carries only the script's `print` and `log.*` lines; notices, prompts, errors and
the log (at `warn` unless `RUST_LOG` says otherwise) go to stderr. `--terminal-demo`
opens the milestone 1 terminal element over an in-memory stream: a development tool.

**Versioned.** No.

**Pinned by.** In `cli.rs`: `all_flags_in_both_spellings`,
`virtual_without_a_name_just_enables_the_simulator`,
`ports_and_virtual_devices_open_in_the_order_given`,
`tcp_and_replay_ids_open_in_the_order_given_with_their_query_intact`,
`the_help_names_every_kind_of_port_id`, `custom_baud_rates_are_accepted`,
`terminal_demo_flag`, `help_and_version`, `errors_name_the_problem`. In `headless.rs`:
`a_script_expects_ok_from_virtual_at_and_exits_zero`, `an_error_exits_one_and_names_the_line`
(which also checks that `Stopped` maps to 2), `the_device_profile_and_baud_set_the_line`,
`without_a_port_or_a_readable_script_nothing_runs`. In `wiring.rs`:
`named_virtual_device_is_listed_with_real_ports_and_opened`.

Not pinned: everything in `main.rs`, which the coverage run leaves out. No test checks
that a bad flag or a setup failure exits 2, or the text `--version` prints. No headless
path stops a script: `run_headless` calls neither `ScriptRun::stop` nor
`ScriptHost::stop_current`, and there is no signal handler, so exit status 2 for
"stopped" is unreachable there, and Ctrl-C ends the process by the signal.

**Proposed promise.** Every flag keeps its name, spelling and meaning in every 1.x; new
flags may be added. The `--script` exit statuses keep their meanings, and headless stdout
carries only the script's output. `--terminal-demo` is not covered. The simulated device
names stay; what the devices send is not promised.

**Before 1.0.** Remove `--terminal-demo` or mark it as unsupported in `--help`. Decide
whether a usage error should keep sharing status 2 with a stopped script. Add an
integration test that runs the binary and checks its exit statuses and `--version`
output.

## 14. Port ids

**Defined in.** The grammar is in the module docs of `crates/serialist-core/src/address.rs`
(`PortAddress::parse`, `TcpAddress`, `ReplayAddress`, `ReplaySpeed`, `ReplayEnd`); schemes
are told apart by `scheme_of` in `composite.rs`.

A port id is an OS path with no scheme (`/dev/cu.usbserial-1420`, `COM3`), `virtual:<name>`,
`tcp:<host>:<port>` (a bracketed IPv6 host; `tcp://` tolerated, the canonical form has no
`//`), or `replay:<path>[?speed=…&end=…]` (`speed` is `<factor>x`, a bare factor or `max`;
`end` is `disconnect` or `hold`; options follow the last `?`). A scheme is two or more
characters, starting with a letter, of letters, digits, `+`, `-` and `.`, so a device path
with colons or a drive letter is never one. Ids are stored in `state.json`, matched by
prefix against a device profile's `match.path`, returned by a script's `port:id()`, and
taken by `--port` and `serial.open{ port = … }`. A relative `replay:` path is relative to
the working directory.

**Versioned.** No.

**Newer and older.** An unknown scheme is refused by the router. In a `replay:` id, text
after the last `?` that has the `key=value` shape is options, and an unknown key or a bad
value is an error, so an id with an option a later build added fails to open in an earlier
one.

**Pinned by.** `schemes_pick_the_address_kind`, `tcp_ids`,
`tcp_ids_round_trip_in_canonical_form`, `replay_ids`, `replay_ids_round_trip`, `speeds`,
`speeds_and_ends_in_settings_json` (`address.rs`); `routes_by_scheme_and_rejects_unknown_ones`
and `schemes` (`composite.rs`); `options_come_from_the_id_then_the_defaults`
(`replay.rs`).

**Proposed promise.** Every id 1.0 accepts means the same in every 1.x. New schemes and
new `replay:` options may be added.

**Before 1.0.** Nothing.

## 15. Recordings and the `.timing` sidecar

**Defined in.** The format is in the module docs of `crates/serialist-core/src/capture.rs`
(`TIMING_MAGIC`, `TIMING_VERSION`, `TimingWriter`, `Timing::read`, `parse_record`); the
recorder is `crates/serialist-ui/src/capture.rs`; replay is
`crates/serialist-core/src/replay.rs`.

The raw file holds every received byte as the transport delivered it and nothing else (no
sent bytes); Record creates or truncates it. The sidecar, `<raw file name>.timing`, is
UTF-8 text: the header `serialist-timing 1`, then `rx <t> <offset> <len>`,
`connect <t> <description>` and `disconnect <t>` records, `#` comments and blank lines.
`t` is microseconds since the recording started.

**Versioned.** Yes: the header's version, 1.

**Newer and older.** A different version in the header is an error, and a replay whose
sidecar does not parse fails to open with a message naming the port. A record kind this
build does not know is skipped only if it is lowercase ASCII letters and `_` (the rule is
in `parse_record`, not in the module docs); any other unknown first word is an error. A
known record with an extra field is an error (`rx: too many fields`), so version 1 can
grow only by new record kinds. A capture without a sidecar replays at the port's baud rate
times the speed.

**Pinned by.** `comments_blank_lines_crlf_and_unknown_kinds_are_skipped`,
`bad_sidecars_are_errors` (version 2 refused, `rx 5 0 1 9` and `RX 5 0 1` errors),
`a_cut_short_last_line_is_tolerated`, `an_unterminated_last_line_is_dropped_even_when_it_parses`,
`written_records_read_back`, `round_trip` (`capture.rs`); the committed capture
`race-session.bin` and its sidecar through `the_fixture_is_the_recorded_device` and
`replay_keeps_the_recorded_chunk_boundaries`
(`crates/serialist-plugins/tests/replay_conformance.rs`); `no_sidecar_paces_at_the_baud`
and the rest of `crates/serialist-core/tests/replay.rs`;
`recording_writes_a_replayable_capture` (`stream_tests.rs`).

**Proposed promise.** Every 1.x reads version 1 sidecars and every raw capture. The raw
file stays received bytes only. A 1.x build writes version 1, or, if it ever writes a
version 2, still reads version 1.

**Before 1.0.** Put the lowercase rule for unknown record kinds into the module docs.
Decide whether sent bytes should be recorded; a `tx` kind fits version 1.

## 16. Exports

**Defined in.** `crates/serialist-ui/src/export.rs` (`ExportFormat`, `ExportFormat::from_path`,
the CSV and JSON writers); `write_lines`, `format_timestamp` and `TextOptions` in
`crates/serialist-core/src/store/export.rs`; `FrameTime`, `direction_label` and
`value_json` in `crates/serialist-ui/src/codecs.rs`.

The format follows the file's extension: `.bin` and `.raw` are raw bytes as received (what
the store still holds), `.txt`, `.log` and `.text` are the lines as displayed, one per line,
optionally stamped `[<stamp>] ` with no Rx or Tx marker; `.csv` and `.json` are decoded
frames. VT mode adds the screen's rows as text. CSV columns are `time`, `direction`, `kind`,
`summary`, one per field name in the order fields first appear, and `raw` (hex). JSON is an
array of objects with `id`, `time`, `direction`, `kind`, `severity`, `summary`, `fields`
(numbers as numbers, bytes as hex, lists as arrays), `raw_start`, `raw_end` and `raw` (hex,
or `null` once evicted). `direction` is `RX`, `TX` or `--`. `time` is the Decoded panel's
stamp: the session's timestamp mode and `display.timestamp_format`, local time, with
absolute stamps while the gutter is off. With the defaults that is `%H:%M:%S%.3f`, no date.

**Versioned.** No.

**Pinned by.** `format_follows_the_extension`, `decoded_frames_export_as_csv_and_json`,
`text_export_writes_lines_as_displayed_with_the_chosen_stamps`,
`raw_export_is_the_stream_byte_for_byte_and_counts_what_was_evicted`,
`a_failed_export_leaves_no_partial_file_and_keeps_the_old_one` (`export.rs`);
`raw_export_is_exactly_what_the_device_sent` (`stream_tests.rs`); the stamp formats in
`crates/serialist-core/src/store/export.rs` (`relative_and_delta_are_seconds_with_microseconds`,
`absolute_takes_any_strftime_format`, `export_stamps_are_bracketed_and_use_the_options_format`).

**Proposed promise.** Raw export stays byte for byte. The CSV columns and the JSON keys
keep their names and meanings; new CSV columns go after the field columns and new JSON
keys may be added. Text and screen exports are for people and are not promised.

**Before 1.0.** Give decoded-frame exports a machine-readable time that does not depend
on display settings (for example an ISO 8601 UTC stamp with the date). Decide whether the
JSON stays a bare array; turning it into an object with a version later would break every
reader.

## 17. The Rust crates

**Defined in.** The workspace `Cargo.toml` and each crate's manifest. The crates are
`serialist`, `serialist-ui`, `serialist-core`, `serialist-script`, `serialist-plugins`,
`serialist-sim`, `serialist-vt` and `serialist-plugin-sdk`, plus
`examples/plugins/airoha-race-wasm` and a test plugin.

**Versioned.** Every crate takes the workspace version, 0.2.0.

None is published. Only `airoha-race-wasm` says `publish = false`; the others could be
packaged, but the workspace dependencies are path-only (`serialist-core = { path =
"crates/serialist-core" }`), so `cargo publish` refuses every crate that depends on
another workspace crate, and `serialist-ui` pins the community GPUI snapshot `gpui-pre
=0.3.7`. No document says any of them is meant for others. `serialist_core::Codec` is the
in-process codec boundary, but third-party codecs reach it only through the Lua and
WebAssembly adapters.

**Proposed promise.** The Rust API of every crate except possibly `serialist-plugin-sdk`
(section 12) is internal and may change in any release.

**Before 1.0.** Add `publish = false` to every crate the promise leaves internal, so an
accidental publish cannot create an expectation.

## Where the documents and the code differed

Found while writing this page and fixed in their own commits:

- `docs/manual.md`'s key binding table, which says it lists the bindings the app ships
  with, left out `command_palette::Toggle` and the palette input's `SelectPrevious` and
  `SelectNext`.
- `docs/plugins.md` and the `serialist-plugin-sdk` crate docs gave the SDK dependency as
  `"0.1"`; the crate is 0.2.0, which `"0.1"` does not match.
- `docs/plugins.md` said a Lua frame's `kind` is "normally" one `describe` lists; both
  adapters turn any other kind into a `plugin_error` frame, and both drop frames out of
  order with the rest of their call. It now says so for both tiers.
- `docs/scripting.md`'s exit status table left out `--script` with more than one `--port`,
  which exits 2.

Not fixed, because they are not errors in a document this change may edit:

- The plan's answer to "Should the plugin API be public and versioned" says a plugin's
  `plugin.toml` declares `api = "1"`; Lua plugins have no `plugin.toml` and no API number
  (section 10).
- `docs/scripting.md` and `--help` say `--script` exits 2 "if it is stopped"; the mapping
  exists (`exit_code`), but no headless path stops a script (section 13).
- The `--help` text for `--config-dir` leaves out `plugins/` and `state.json` (section 1).

## Decisions for the owner

1. **How a later 1.x file meets an earlier 1.x build.** Today one unknown value rejects a
   whole `settings.json` (the earlier build drops every user setting), one unknown payload
   form or param kind drops a whole command collection, and an edit in the earlier build
   deletes keys it does not know. Change the loaders and the collection writer before 1.0,
   or promise that 1.x only ever adds keys, never values.
2. **Whether the WebAssembly tier is in 1.0.** The release packages are built without the
   `wasm` feature, so `wit/v1`, `plugin.toml` and the SDK reach only people who build from
   source. Ship the feature in the packages, or leave the tier out of the promise.
3. **Which Rust crates are public.** The proposal: none, except possibly
   `serialist-plugin-sdk`, and `publish = false` on the rest. Publishing the SDK needs the
   WIT inside the crate and a version line of its own.
4. **Whether Lua plugins declare an API before 1.0.** Without a declared number, a later
   incompatible Lua codec interface cannot tell old plugins from new ones.
5. **What the Lua scripting API is at 1.0.** `codecs.<name>.encode`, timers and
   `port:on_frame` are in now; what is left is whether scripts get a version value to
   check.
6. **Whether defaults are promised.** Default setting values, default key chords and the
   bundled theme names reach every user who has not overridden them; promise them, or
   promise only file formats and names and announce default changes in the changelog.
7. **The config directory.** Keep `~/.config/serialist` on macOS and ignore
   `XDG_CONFIG_HOME` on Linux, or change either now; after 1.0 a move needs a migration.
8. **Decoded-frame exports.** Add a machine-readable time and settle the JSON's top-level
   shape before promising the columns and keys.
9. **The command line's loose ends.** Remove or disown `--terminal-demo`, and decide whether
   a usage error keeps sharing exit status 2 with a stopped script.
10. **`state.json` after a downgrade.** Whether an earlier build may overwrite a later
    build's state file at quit.
