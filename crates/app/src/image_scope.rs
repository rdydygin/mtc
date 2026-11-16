//! Drawing part of a window with images from a cache other than GPUI's
//! app-wide store, which keeps every image for the whole run.
//!
//! GPUI's `image_cache` element makes its cache current only while its
//! children are laid out and painted. Elements that lay out children while
//! they prepaint, such as a virtual list's rows or a text view's pictures,
//! and deferred popover content, which is painted after the rest of the
//! window, would then load into the app-wide store as well. [`ImageScope`]
//! keeps the cache current in all three phases, and adds no layout node of
//! its own.
//!
//! A scope also stops gpui-fast from building the views nested in it again
//! on their own, so scopes go around rows and popover content, never around
//! a view that changes often, such as the composer.

use std::{collections::HashMap, panic::Location, sync::Arc};

use gpui_kit::{
    AnyElement, AnyImageCache, App, AppContext as _, Bounds, Element, ElementId, Entity,
    GlobalElementId, ImageCache, ImageCacheError, ImageCacheItem, InspectorElementId, IntoElement,
    LayoutId, Pixels, RenderImage, Resource, Window,
};

/// The pictures decoded for one part of the interface, such as a
/// conversation or the image viewer, kept until it goes and then freed from
/// memory and the GPU atlas. GPUI's `RetainAllImageCache` does the same,
/// but cannot say what it holds.
pub(crate) struct ImageStore {
    images: HashMap<Resource, ImageCacheItem>,
}

impl ImageStore {
    pub(crate) fn new(cx: &mut App) -> Entity<Self> {
        let store = cx.new(|_| Self {
            images: HashMap::new(),
        });
        cx.observe_release(&store, |store, cx| {
            for (_, item) in store.images.drain() {
                if let Some(Ok(image)) = item.get() {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        store
    }

    /// Free the pictures loaded from resources `keep` turns down, from
    /// memory and the GPU atlas, for a part of the interface that outlives
    /// what it shows. Loads still running are cancelled.
    pub(crate) fn retain(&mut self, keep: impl Fn(&Resource) -> bool, cx: &mut App) {
        let mut freed = Vec::new();
        self.images.retain(|resource, item| {
            if keep(resource) {
                return true;
            }
            freed.extend(item.get().and_then(Result::ok));
            false
        });
        // A window being updated is out of GPUI's list until the update
        // ends, and drop_image only reaches the windows in it.
        if !freed.is_empty() {
            cx.defer(move |cx| {
                for image in freed {
                    cx.drop_image(image, None);
                }
            });
        }
    }

    /// The picture at `resource`: `None` while it loads or if it was never
    /// asked for.
    #[cfg(test)]
    pub(crate) fn get(
        &self,
        resource: &Resource,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        self.images.get(resource)?.get()
    }
}

impl ImageCache for ImageStore {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        self.images
            .entry(resource.clone())
            .or_insert_with(|| ImageCacheItem::new(resource, cx))
            .use_image(window)
    }
}

/// `child`, drawn with its images held by `images`.
pub(crate) fn image_scope(images: &Entity<ImageStore>, child: impl IntoElement) -> ImageScope {
    ImageScope {
        images: images.clone().into(),
        child: child.into_any_element(),
    }
}

/// `content` in `images` when there is one. Popover content is laid out
/// where its trigger is but painted after the rest of the window, so it
/// names its cache rather than relying on one around the trigger.
pub(crate) fn in_image_scope(
    images: Option<&Entity<ImageStore>>,
    content: impl IntoElement,
) -> AnyElement {
    match images {
        Some(images) => image_scope(images, content).into_any_element(),
        None => content.into_any_element(),
    }
}

pub(crate) struct ImageScope {
    images: AnyImageCache,
    child: AnyElement,
}

impl IntoElement for ImageScope {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ImageScope {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let child = &mut self.child;
        let layout_id = window.with_image_cache(Some(self.images.clone()), |window| {
            child.request_layout(window, cx)
        });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        window.with_image_cache(Some(self.images.clone()), |window| {
            child.prepaint(window, cx)
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        window.with_image_cache(Some(self.images.clone()), |window| child.paint(window, cx));
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, ImageCacheError, RenderImage, TestAppContext,
    };

    /// Draw `window` until every picture `pictures` looks up has decoded and
    /// been painted into the window's GPU atlas, and return them. Pictures
    /// served by [`crate::http::PictureClient`] load on the test executor,
    /// so a few frames settle them.
    pub(crate) fn draw_until_painted(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        pictures: impl Fn(&App) -> Vec<Option<Result<Arc<RenderImage>, ImageCacheError>>>,
    ) -> Vec<Arc<RenderImage>> {
        use gpui_kit::test::TestWindowExt as _;
        for _ in 0..20 {
            cx.run_until_parked();
            let painted = cx
                .update_window(window, |_, window, cx| {
                    window.render_frame(cx);
                    pictures(cx)
                        .into_iter()
                        .map(|picture| match picture {
                            Some(Ok(image)) if window.has_image_atlas_entry(&image) => Some(image),
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .unwrap();
            if let Some(painted) = painted {
                return painted;
            }
        }
        let states = cx.update(|cx| {
            pictures(cx)
                .into_iter()
                .map(|picture| picture.map(|result| result.is_ok()))
                .collect::<Vec<_>>()
        });
        panic!("pictures never painted from their store: {states:?}");
    }

    /// Whether `window`'s GPU atlas still holds `image`.
    pub(crate) fn in_atlas(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        image: &RenderImage,
    ) -> bool {
        cx.update_window(window, |_, window, _| window.has_image_atlas_entry(image))
            .unwrap()
    }
}
