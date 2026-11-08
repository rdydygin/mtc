use std::{collections::HashMap, sync::Arc};

/// The server's custom emoji, by name, with the URL of each image.
///
/// A cheap-to-clone snapshot: the [`crate::Session`] replaces it as emoji
/// load or arrive, and hands the new one to every live timeline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomEmojis {
    urls: Arc<HashMap<String, Arc<str>>>,
}

impl CustomEmojis {
    /// A set of `(name, image URL)` pairs.
    pub fn new(emoji: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            urls: Arc::new(
                emoji
                    .into_iter()
                    .map(|(name, url)| (name, url.into()))
                    .collect(),
            ),
        }
    }

    pub fn image_url(&self, name: &str) -> Option<&Arc<str>> {
        self.urls.get(name)
    }

    pub fn insert(&mut self, name: String, url: String) {
        Arc::make_mut(&mut self.urls).insert(name, url.into());
    }

    pub fn len(&self) -> usize {
        self.urls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.urls.is_empty()
    }
}
