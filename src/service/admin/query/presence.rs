use clap::Subcommand;
use ruma::{events::room::message::RoomMessageEventContent, UserId};

use crate::{services, Result};

#[cfg_attr(test, derive(Debug))]
#[derive(Subcommand)]
/// All the getters and iterators from src/database/key_value/presence.rs
pub(crate) enum Presence {
	/// - Returns the latest presence event for the given user.
	GetPresence {
		/// Full user ID
		user_id: Box<UserId>,
	},

	/// - Iterator of the most recent presence updates that happened after the
	///   event with id `since`.
	PresenceSince {
		/// UNIX timestamp since (u64)
		since: u64,
	},
}

/// All the getters and iterators in key_value/presence.rs
pub(super) async fn presence(subcommand: Presence) -> Result<RoomMessageEventContent> {
	match subcommand {
		Presence::GetPresence {
			user_id,
		} => {
			let timer = tokio::time::Instant::now();
			let results = services().presence.db.get_presence(&user_id)?;
			let query_time = timer.elapsed();

			Ok(RoomMessageEventContent::text_html(
				format!("Query completed in {query_time:?}:\n\n```\n{:?}```", results),
				format!(
					"<p>Query completed in {query_time:?}:</p>\n<pre><code>{:?}\n</code></pre>",
					results
				),
			))
		},
		Presence::PresenceSince {
			since,
		} => {
			let timer = tokio::time::Instant::now();
			let results = services().presence.db.presence_since(since);
			let query_time = timer.elapsed();

			let presence_since: Vec<(_, _, _)> = results.collect();

			Ok(RoomMessageEventContent::text_html(
				format!("Query completed in {query_time:?}:\n\n```\n{:?}```", presence_since),
				format!(
					"<p>Query completed in {query_time:?}:</p>\n<pre><code>{:?}\n</code></pre>",
					presence_since
				),
			))
		},
	}
}
