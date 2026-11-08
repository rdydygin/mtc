//! The signed-in Mattermost session as GPUI entities.
//!
//! - [`Session`] owns teams, channels, read state, and the real-time
//!   connection, and routes server events to the right place.
//! - [`Directory`] resolves users and presence with batched requests.
//! - [`Timeline`] holds the ordered posts of one channel or thread.
//! - [`EmojiStore`] looks up the server's custom emoji as they appear;
//!   [`CustomEmojis`] is its snapshot, mapping names to images.
//!
//! Nothing here renders; the UI crate mirrors these models.

mod directory;
mod emoji;
mod prepare;
mod retry;
mod session;
mod timeline;

pub use directory::Directory;
pub use emoji::{CustomEmojis, EmojiStore};
pub use prepare::{
    PreparedMessage, custom_emoji_names, emoji_for, emoji_name, emojify, needs_rich_text,
};
pub use session::{ChannelEntry, Connection, Session, SessionEvent};
pub use timeline::{Delivery, Entry, LoadState, PAGE_SIZE, Timeline, TimelineEvent, TimelineKind};
