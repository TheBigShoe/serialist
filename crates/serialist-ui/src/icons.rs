//! The app's icon set: gpui-kit's bundled component icons plus the Lucide icons the
//! chrome draws (the toolbar, the dock rails, the rows' hover actions).
//!
//! gpui-kit's [`KitAssets`] embeds only the icons its own components use (chevrons,
//! check marks, the dialog's close button). [`IconName`] names the whole Lucide catalog,
//! but an icon draws only if the registered asset source has its SVG, so [`Assets`]
//! adds the ones listed in [`AppIcons`] (embedded from gpui-kit's catalog, a few hundred
//! bytes each) in front of gpui-kit's own. Register it with `Application::with_assets`;
//! the binary and the screenshot harness both do.

use std::borrow::Cow;

use crate::prelude::*;

icon_assets!(pub AppIcons, [
    Activity,
    ArrowDownToLine,
    Binary,
    Braces,
    Cable,
    Circle,
    CircleDot,
    CircleStop,
    Clock,
    Command,
    CornerDownLeft,
    Download,
    Eraser,
    FileCode,
    FileDown,
    FileText,
    FolderOpen,
    Keyboard,
    ListPlus,
    MessageSquare,
    PanelLeft,
    PanelRight,
    Pencil,
    Play,
    Plug,
    ScrollText,
    SendHorizontal,
    SquareTerminal,
    TextWrap,
    Unplug,
    Usb,
    X,
]);

/// The icons the app draws: [`AppIcons`], then gpui-kit's bundled set.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match AppIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => KitAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = AppIcons.list(path)?;
        paths.extend(KitAssets.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon the chrome names loads from the registered source.
    #[test]
    fn the_chrome_icons_load() {
        for icon in [
            IconName::Usb,
            IconName::SquareTerminal,
            IconName::Braces,
            IconName::ScrollText,
            IconName::Plug,
            IconName::Unplug,
            IconName::TextWrap,
            IconName::Search,
            IconName::Ellipsis,
            IconName::ChevronDown,
            IconName::Settings2,
        ] {
            let path = icon.path();
            let loaded = Assets.load(&path).expect("no load error");
            assert!(loaded.is_some_and(|bytes| !bytes.is_empty()), "{path}");
        }
        assert!(
            Assets
                .list("icons/")
                .unwrap()
                .contains(&SharedString::from("icons/text-wrap.svg"))
        );
    }
}
