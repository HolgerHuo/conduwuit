mod commands;

use clap::Subcommand;
use conduwuit::Result;
use ruma::{RoomId, ServerName, UserId};

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(super) enum FederationCommand {
	/// - List all rooms we are currently handling an incoming pdu from
	IncomingFederation,

	/// - Disables incoming federation handling for a room.
	DisableRoom {
		room_id: Box<RoomId>,
	},

	/// - Enables incoming federation handling for a room again.
	EnableRoom {
		room_id: Box<RoomId>,
	},

	/// - Fetch `/.well-known/matrix/support` from the specified server
	///
	/// Despite the name, this is not a federation endpoint and does not go
	/// through the federation / server resolution process as per-spec this is
	/// supposed to be served at the server_name.
	///
	/// Respecting homeservers put this file here for listing administration,
	/// moderation, and security inquiries. This command provides a way to
	/// easily fetch that information.
	FetchSupportWellKnown {
		server_name: Box<ServerName>,
	},

	/// - Lists all the rooms we share/track with the specified *remote* user
	RemoteUserInRooms {
		user_id: Box<UserId>,
	},
}
