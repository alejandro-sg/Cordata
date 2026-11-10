//! Cordata's own icons (`app/assets/icons`, rules in `app/assets/brand/README.md`): a 16 grid, one 1.25 stroke with
//! round ends, and the mark's two parts — an open line and, where something is set, a solid node. They answer to
//! `gpui_kit_assets::IconName` so the component library's `Icon` draws them; a name not listed here loads nothing.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};
use gpui_kit_assets::IconName;

macro_rules! icons {
    ($($name:ident => $file:literal),* $(,)?) => {
        const SET: &[(IconName, &[u8])] = &[$((IconName::$name, include_bytes!(concat!("../../../assets/icons/", $file, ".svg")))),*];
    };
}

icons! {
    // Whole controls (each has a tooltip).
    Search => "search",
    PanelLeft => "panel-left",
    PanelRight => "panel-right",
    SquarePen => "new-thread",
    X => "close",
    Settings => "settings",
    Paperclip => "attach",
    Mic => "dictate",
    RefreshCw => "refresh",
    // States you would otherwise read: a folder or a select, open or closed.
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    // Rope redesign additions:
    BookOpen => "logbook",
    Sheet => "sheet",
    Terminal => "terminal",
    Files => "files",
    GitPullRequest => "review",
    ShieldAlert => "gate",
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(SET.iter().find(|(name, _)| name.path().as_ref() == path).map(|(_, svg)| Cow::Borrowed(*svg)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(SET.iter().map(|(name, _)| name.path()).filter(|p| p.starts_with(path)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_on_the_grid_in_one_stroke() {
        assert_eq!(SET.len(), 17, "expected 17 icons in the rope icon set");
        for (name, svg) in SET {
            let svg = std::str::from_utf8(svg).unwrap();
            assert!(svg.contains(r#"viewBox="0 0 16 16""#), "{name:?} is off the 16 grid");
            assert!(svg.contains(r#"stroke-width="1.25""#) && svg.contains(r#"stroke-linecap="round""#), "{name:?} is not the set's stroke");
            assert!(svg.contains("currentColor") && !svg.contains('#'), "{name:?} carries its own colour");
        }
    }

    #[test]
    fn a_listed_icon_loads_and_an_unlisted_one_does_not() {
        assert!(Icons.load(IconName::Search.path().as_ref()).unwrap().is_some());
        assert!(Icons.load(IconName::BookOpen.path().as_ref()).unwrap().is_some());
        assert!(Icons.load(IconName::Sheet.path().as_ref()).unwrap().is_some());
        assert!(Icons.load(IconName::Terminal.path().as_ref()).unwrap().is_some());
        assert!(Icons.load(IconName::Files.path().as_ref()).unwrap().is_some());
        assert!(Icons.load(IconName::GitPullRequest.path().as_ref()).unwrap().is_some());
        assert!(Icons.load(IconName::ShieldAlert.path().as_ref()).unwrap().is_some());
        assert!(Icons.load(IconName::Check.path().as_ref()).unwrap().is_none());
    }
}
