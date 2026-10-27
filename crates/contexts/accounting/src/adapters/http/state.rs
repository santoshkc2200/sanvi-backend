use std::sync::Arc;

use crate::application::{
    ActivateBook, CancelSwitch, CompleteAuthorization, CreateBook, GetBook, ListBooks,
    RegisterConnection, RevokeAuthorization, ScheduleSwitch, StartAuthorization,
};

#[derive(Clone)]
pub struct AccountingState {
    pub list_books: Arc<ListBooks>,
    pub get_book: Arc<GetBook>,
    pub create_book: Arc<CreateBook>,
    pub register_connection: Arc<RegisterConnection>,
    pub activate_book: Arc<ActivateBook>,
    pub schedule_switch: Arc<ScheduleSwitch>,
    pub cancel_switch: Arc<CancelSwitch>,
    /// `None` until freee secrets are deployed; consent routes answer 503.
    pub consent: Option<Consent>,
}

impl std::fmt::Debug for AccountingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountingState")
            .field("has_consent", &self.consent.is_some())
            .finish()
    }
}

#[derive(Clone)]
pub struct Consent {
    pub start: Arc<StartAuthorization>,
    pub complete: Arc<CompleteAuthorization>,
    pub revoke: Arc<RevokeAuthorization>,
}

impl std::fmt::Debug for Consent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consent").finish()
    }
}
