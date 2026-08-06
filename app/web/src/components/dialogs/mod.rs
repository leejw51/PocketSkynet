//! The dialog layer: Create room, Invite, Manage admins, Blocked people,
//! Hidden rooms, Rename room, Delete message.
//!
//! One module per dialog, but they are grouped here because they share one
//! shape — a `.fn-modal` with a `[secondary] [primary]` footer — and keeping
//! them adjacent is what keeps the copy consistent, which is most of what makes
//! a dialog trustworthy.
//!
//! Every destructive confirmation names the object and states the consequence,
//! and its button is labelled with the verb. `window.confirm` appears nowhere.

mod admin;
mod admins;
mod assistant;
mod blocked;
mod create_room;
mod delete_message;
mod files;
mod hidden;
mod invite;
mod mentions;
mod more;
mod new_dm;
mod rename;
mod server;
mod wallet;

pub use admin::AdminConsole;
pub use admins::ManageAdmins;
pub use assistant::{AiKeysEditor, Assistant};
pub use blocked::Blocked;
pub use create_room::CreateRoom;
pub use delete_message::DeleteMessage;
pub use files::Files;
pub use hidden::HiddenRooms;
pub use invite::Invite;
pub use mentions::Mentions;
pub use more::MoreSheet;
pub use new_dm::NewDirectMessage;
pub use rename::RenameRoom;
pub use server::ServerInfoDialog;
pub use wallet::Wallet;
