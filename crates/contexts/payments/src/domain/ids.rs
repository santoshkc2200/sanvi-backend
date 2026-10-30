//! Typed identifiers of Payments records. The service mints them; the
//! platform sees them in paths, response bodies and inbound events.

use sanvi_kernel::Id;

pub enum ConnectionTag {}
pub type ConnectionId = Id<ConnectionTag>;

pub enum CheckoutTag {}
pub type CheckoutId = Id<CheckoutTag>;

pub enum PaymentTag {}
pub type PaymentId = Id<PaymentTag>;

pub enum RefundTag {}
pub type RefundId = Id<RefundTag>;

pub enum DisputeTag {}
pub type DisputeId = Id<DisputeTag>;

pub enum PayoutTag {}
pub type PayoutId = Id<PayoutTag>;
