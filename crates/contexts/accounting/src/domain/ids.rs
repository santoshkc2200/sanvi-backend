//! Typed identifiers of Accounting records. The service mints them; the
//! platform sees them in paths, response bodies and internal deliveries.

use sanvi_kernel::Id;

pub enum BookTag {}
pub type BookId = Id<BookTag>;

pub enum ConnectionTag {}
pub type ConnectionId = Id<ConnectionTag>;
