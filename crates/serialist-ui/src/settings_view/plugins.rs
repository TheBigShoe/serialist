//! The Plugins section: the codec plugins installed in the plugins folder, with their
//! kind and version, the problems the last load had, the bundled examples that are not
//! installed, and buttons to reload the folder and open it.

use super::SettingsView;
use crate::chrome;
use crate::config::{self, Config, ConfigPiece};
use crate::plugin_files;
use crate::prelude::*;

/// One installed plugin as the section lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRow {
    /// The folder's name, which profiles and commands use.
    pub name: String,
    /// `Lua` or `WebAssembly`.
    pub kind: &'static str,
    pub version: String,
    pub description: String,
}

/// The installed plugins, by folder name.
pub fn plugin_rows(cx: &App) -> Vec<PluginRow> {
    let Some(config) = cx.try_global::<Config>() else {
        return Vec::new();
    };
    config
        .codecs()
        .plugins()
        .iter()
        .map(|plugin| {
            let info = plugin.factory.info();
            let kind = match plugin.entry.extension().and_then(|ext| ext.to_str()) {
                Some("wasm") => "WebAssembly",
                _ => "Lua",
            };
            PluginRow {
                name: plugin.name.clone(),
                kind,
                version: info.version,
                description: info.description,
            }
        })
        .collect()
}

impl SettingsView {
    /// Install the bundled example plugin `name`; the watcher then loads it.
    pub fn install_example_plugin(&mut self, name: &str, cx: &mut Context<Self>) {
        let notice = plugin_files::install_example(name, cx);
        self.plugin_notice = Some((notice.text, notice.is_error));
        cx.notify();
    }

    pub(super) fn render_plugins(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let rows = plugin_rows(cx);
        let problems: Vec<String> = cx
            .try_global::<Config>()
            .map(|config| {
                config
                    .problems()
                    .iter()
                    .filter(|problem| problem.piece == ConfigPiece::Plugins)
                    .map(|problem| problem.message.clone())
                    .collect()
            })
            .unwrap_or_default();
        let examples = plugin_files::examples_to_install(cx);
        let folder = cx
            .try_global::<Config>()
            .map(|config| config.paths().plugins_dir().display().to_string())
            .unwrap_or_default();
        let theme = cx.theme();
        let (muted, danger, border, info) =
            (theme.muted_foreground, theme.danger, theme.border, theme.info);

        let mut list: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                h_flex()
                    .id(("settings-plugin", ix))
                    .test_support()
                    .w_full()
                    .h(chrome::TWO_LINE_ROW_HEIGHT)
                    .px_1()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .child(SharedString::from(row.name.clone())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(muted)
                                    .child(SharedString::from(row.description.clone())),
                            ),
                    )
                    .child(chrome::chip(info).child(row.kind))
                    .child(chrome::quiet_chip(format!("v{}", row.version), cx))
                    .into_any_element()
            })
            .collect();
        if list.is_empty() {
            list.push(
                div()
                    .py_2()
                    .text_sm()
                    .text_color(muted)
                    .child("No plugin is installed, so nothing is decoded.")
                    .into_any_element(),
            );
        }
        let actions = h_flex()
            .gap_2()
            .pt_1()
            .child(
                Button::new("settings-plugins-reload")
                    .icon(IconName::RefreshCw)
                    .label("Reload")
                    .small()
                    .on_click(|_, _, cx| config::reload(ConfigPiece::Plugins, cx)),
            )
            .child(
                Button::new("settings-plugins-folder")
                    .icon(IconName::FolderOpen)
                    .label("Open folder")
                    .small()
                    .ghost()
                    .on_click(|_, _, cx| plugin_files::open_plugins_folder(cx)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(folder)),
            );
        let example_rows: Vec<AnyElement> = examples
            .iter()
            .map(|example| {
                let name = example.name;
                h_flex()
                    .w_full()
                    .h(chrome::TWO_LINE_ROW_HEIGHT)
                    .px_1()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(example.title))
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(muted)
                                    .child(example.description),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("settings-install-{name}")))
                            .icon(IconName::Download)
                            .label("Install")
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.install_example_plugin(name, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        let notice = self.plugin_notice.clone();
        let mut groups = vec![
            v_flex()
                .w_full()
                .gap_2()
                .pt_4()
                .child(chrome::section_label("Installed", cx))
                .child(
                    v_flex()
                        .w_full()
                        .py_1()
                        .border_t_1()
                        .border_b_1()
                        .border_color(border)
                        .children(list),
                )
                .children(problems.into_iter().map(|problem| {
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child(SharedString::from(problem))
                }))
                .child(actions)
                .into_any_element(),
        ];
        if !example_rows.is_empty() || notice.is_some() {
            groups.push(
                v_flex()
                    .w_full()
                    .gap_2()
                    .pt_4()
                    .child(chrome::section_label("Examples", cx))
                    .children(example_rows)
                    .children(notice.map(|(text, is_error)| {
                        div()
                            .id("settings-plugins-notice")
                            .test_support()
                            .text_xs()
                            .text_color(if is_error { danger } else { muted })
                            .child(SharedString::from(text))
                    }))
                    .into_any_element(),
            );
        }
        groups
    }
}
