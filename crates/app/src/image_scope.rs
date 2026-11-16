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

use std::{collections::HashMap, panic::Location};

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

    /// The picture at `resource`: `None` while it loads or if it was never
    /// asked for.
    #[cfg(test)]
    pub(crate) fn get(
        &self,
        resource: &Resource,
    ) -> Option<Result<std::sync::Arc<RenderImage>, ImageCacheError>> {
        self.images.get(resource)?.get()
    }
}

impl ImageCache for ImageStore {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<std::sync::Arc<RenderImage>, ImageCacheError>> {
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
