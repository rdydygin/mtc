//! The signed-in Mattermost session as GPUI entities.
//!
//! - [`Session`] owns teams, channels, read state, and the real-time
//!   connection, and routes server events to the right place.
//! - [`Directory`] resolves users and presence with batched requests.
//! - [`Timeline`] holds the ordered posts of one channel or thread.
//! - [`CustomEmojis`] maps the server's custom emoji to their images.
//!
//! Nothing here renders; the UI crate mirrors these models.

mod directory;
mod emoji;
mod prepare;
mod retry;
mod session;
mod timeline;

pub use directory::Directory;
pub use emoji::CustomEmojis;
pub use prepare::{PreparedMessage, emoji_for, emojify, needs_rich_text};
pub use session::{ChannelEntry, Connection, Session, SessionEvent};
pub use timeline::{Delivery, Entry, LoadState, PAGE_SIZE, Timeline, TimelineEvent, TimelineKind};
