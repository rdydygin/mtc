//! The signed-in Mattermost session as GPUI entities.
//!
//! - [`Session`] owns teams, channels, read state, and the real-time
//!   connection, and routes server events to the right place.
//! - [`Directory`] resolves users and presence with batched requests.
//! - [`Timeline`] holds the ordered posts of one channel or thread.
//! - [`FollowedThreads`] lists the threads the user follows, for the
//!   Threads pane, with their unread totals.
//! - [`EmojiStore`] looks up the server's custom emoji as they appear;
//!   [`CustomEmojis`] is its snapshot, mapping names to images.
//!
//! Nothing here renders; the UI crate mirrors these models.

mod directory;
mod emoji;
mod prepare;
mod retry;
mod search;
mod session;
mod threads;
mod timeline;

pub use directory::{Directory, UsernameLookup};
pub use emoji::{CustomEmojis, EmojiStore, emoji_matches};
pub use prepare::{
    PreparedMessage, custom_emoji_names, emoji_for, emoji_name, emojify, mention_username,
    needs_rich_text,
};
pub use search::{SEARCH_PAGE_SIZE, SearchScope, search_terms};
pub use session::{ChannelEntry, Connection, Session, SessionEvent};
pub use threads::{FollowedThread, FollowedThreads, ThreadFilter, ThreadsEvent};
pub use timeline::{Delivery, Entry, LoadState, PAGE_SIZE, Timeline, TimelineEvent, TimelineKind};
