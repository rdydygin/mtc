//! Icons: the component defaults plus the few extra Lucide icons this app
//! draws. Embedding only what is used keeps the binary small.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        ArrowDown,
        AtSign,
        ChevronDown,
        ChevronRight,
        ChevronsUpDown,
        Circle,
        CircleAlert,
        CircleArrowUp,
        CircleDot,
        Ellipsis,
        GitFork,
        Globe,
        Hash,
        KeyRound,
        Keyboard,
        Lock,
        LogOut,
        MessageSquare,
        Moon,
        CodeXml,
        Copy,
        FaceSlightlySmilingPlus,
        Monitor,
        Check,
        Download,
        File,
        Paperclip,
        ArrowUp,
        Pencil,
        Reply,
        Scan,
        Search,
        SendHorizontal,
        Sun,
        SunMoon,
        ThumbsUp,
        Users,
        X,
        ZoomIn,
        ZoomOut,
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = crate::lang_icon::load(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
