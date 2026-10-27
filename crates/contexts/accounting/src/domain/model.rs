use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, Utc};
use sanvi_kernel::{Id, TenantId};
use serde::{Deserialize, Serialize};

use super::AccountingError;

pub const LATE_EVENT_GRACE: TimeDelta = match TimeDelta::try_days(60) {
    Some(delta) => delta,
    None => panic!("invalid late event grace delta"),
};
pub const MAX_BACKFILL: TimeDelta = match TimeDelta::try_days(365) {
    Some(delta) => delta,
    None => panic!("invalid max backfill delta"),
};

pub enum BookTag {}
pub type BookId = Id<BookTag>;
pub enum ConnectionTag {}
pub type ConnectionId = Id<ConnectionTag>;
pub enum PostingTag {}
pub type PostingId = Id<PostingTag>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Freee,
    MoneyForward,
    Xero,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Freee => "freee",
            Self::MoneyForward => "money_forward",
            Self::Xero => "xero",
        }
    }
}

/// Provider account mappings are immutable once a destination is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountMappings {
    pub revenue_account: String,
    pub refund_account: String,
    pub clearing_account: String,
    pub tax_codes: BTreeMap<String, String>,
}

impl AccountMappings {
    pub fn validate(&self) -> Result<(), AccountingError> {
        for value in [
            &self.revenue_account,
            &self.refund_account,
            &self.clearing_account,
        ] {
            if value.trim().is_empty() || value.len() > 256 {
                return Err(AccountingError::Invalid(
                    "account/tax mappings must contain 1–256 bytes",
                ));
            }
        }
        if self.tax_codes.is_empty()
            || self.tax_codes.iter().any(|(source, target)| {
                source.trim().is_empty()
                    || target.trim().is_empty()
                    || source.len() > 256
                    || target.len() > 256
            })
        {
            return Err(AccountingError::Invalid(
                "at least one nonempty source-to-provider tax mapping is required",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    id: ConnectionId,
    provider: Provider,
    organization_id: String,
    mappings: AccountMappings,
}

impl Connection {
    pub fn new(
        id: ConnectionId,
        provider: Provider,
        organization_id: String,
        mappings: AccountMappings,
    ) -> Result<Self, AccountingError> {
        if organization_id.trim().is_empty() || organization_id.len() > 256 {
            return Err(AccountingError::Invalid(
                "organization reference must contain 1–256 bytes",
            ));
        }
        mappings.validate()?;
        Ok(Self {
            id,
            provider,
            organization_id,
            mappings,
        })
    }
    pub fn id(&self) -> ConnectionId {
        self.id
    }
    pub fn provider(&self) -> Provider {
        self.provider
    }
    pub fn organization_id(&self) -> &str {
        &self.organization_id
    }
    pub fn mappings(&self) -> &AccountMappings {
        &self.mappings
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationPeriod {
    pub connection_id: ConnectionId,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
}

/// One tenant-local legal entity has one book and at most one active destination.
/// Old periods and connection mappings remain available for reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    id: BookId,
    tenant_id: TenantId,
    legal_entity_key: String,
    revision: i64,
    connections: Vec<Connection>,
    periods: Vec<DestinationPeriod>,
}

impl Book {
    /// Validate a persisted snapshot before any routing or mutation is allowed.
    pub fn validate(&self) -> Result<(), AccountingError> {
        Self::new(self.id, self.tenant_id, self.legal_entity_key.clone())?;
        if self.revision < 0 {
            return Err(AccountingError::Invalid("negative book revision"));
        }
        for (index, connection) in self.connections.iter().enumerate() {
            Connection::new(
                connection.id,
                connection.provider,
                connection.organization_id.clone(),
                connection.mappings.clone(),
            )?;
            if self.connections[..index]
                .iter()
                .any(|previous| previous.id == connection.id)
            {
                return Err(AccountingError::Invalid("duplicate destination binding"));
            }
        }
        for (index, period) in self.periods.iter().enumerate() {
            self.connection(period.connection_id)?;
            if self.periods[..index]
                .iter()
                .any(|previous| previous.connection_id == period.connection_id)
            {
                return Err(AccountingError::Invalid("destination binding was reused"));
            }
            match self.periods.get(index + 1) {
                Some(next)
                    if period.ends_at == Some(next.starts_at)
                        && next.starts_at > period.starts_at => {}
                None if period.ends_at.is_none() => (),
                _ => return Err(AccountingError::Invalid("invalid destination periods")),
            }
        }
        Ok(())
    }
    pub fn new(
        id: BookId,
        tenant_id: TenantId,
        legal_entity_key: String,
    ) -> Result<Self, AccountingError> {
        if legal_entity_key.trim().is_empty()
            || legal_entity_key.len() > 128
            || legal_entity_key.trim() != legal_entity_key
        {
            return Err(AccountingError::Invalid(
                "legal entity key must contain 1–128 bytes without surrounding whitespace",
            ));
        }
        Ok(Self {
            id,
            tenant_id,
            legal_entity_key,
            revision: 0,
            connections: Vec::new(),
            periods: Vec::new(),
        })
    }
    pub fn id(&self) -> BookId {
        self.id
    }
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }
    pub fn legal_entity_key(&self) -> &str {
        &self.legal_entity_key
    }
    pub fn revision(&self) -> i64 {
        self.revision
    }
    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }
    pub fn periods(&self) -> &[DestinationPeriod] {
        &self.periods
    }
    pub fn active_connection(&self, now: DateTime<Utc>) -> Option<ConnectionId> {
        self.periods
            .iter()
            .find(|period| period.starts_at <= now && period.ends_at.is_none_or(|end| now < end))
            .map(|period| period.connection_id)
    }
    pub fn scheduled_switch(&self, now: DateTime<Utc>) -> Option<&DestinationPeriod> {
        self.periods.last().filter(|period| period.starts_at > now)
    }
    pub fn draining_connections(&self, now: DateTime<Utc>) -> Vec<ConnectionId> {
        self.periods
            .iter()
            .filter_map(|period| {
                let ends_at = period.ends_at?;
                if now < ends_at + LATE_EVENT_GRACE {
                    Some(period.connection_id)
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn connection(&self, id: ConnectionId) -> Result<&Connection, AccountingError> {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
            .ok_or(AccountingError::NotFound)
    }

    pub fn add_connection(&mut self, connection: Connection) -> Result<(), AccountingError> {
        // Deserialize is not proof that constructor validation ran.
        Connection::new(
            connection.id,
            connection.provider,
            connection.organization_id.clone(),
            connection.mappings.clone(),
        )?;
        if self
            .connections
            .iter()
            .any(|existing| existing.id == connection.id)
        {
            return Err(AccountingError::Conflict("destination already registered"));
        }
        self.connections.push(connection);
        self.revision += 1;
        Ok(())
    }

    /// Call only after a trusted provider verifier has checked account access and mappings.
    pub fn activate_initial(
        &mut self,
        connection_id: ConnectionId,
        starts_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), AccountingError> {
        self.connection(connection_id)?;
        if !self.periods.is_empty() {
            return Err(AccountingError::Conflict(
                "an active destination already exists",
            ));
        }
        if starts_at > now {
            return Err(AccountingError::Invalid(
                "initial activation cannot start in the future",
            ));
        }
        if starts_at < now - MAX_BACKFILL {
            return Err(AccountingError::Invalid(
                "activation cannot start more than 365 days in the past",
            ));
        }
        self.periods.push(DestinationPeriod {
            connection_id,
            starts_at,
            ends_at: None,
        });
        self.revision += 1;
        Ok(())
    }

    pub fn schedule_switch(
        &mut self,
        connection_id: ConnectionId,
        cutover_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), AccountingError> {
        self.connection(connection_id)?;
        if self.active_connection(now).is_none() {
            return Err(AccountingError::Conflict("no active destination"));
        }
        if self.scheduled_switch(now).is_some() {
            return Err(AccountingError::Conflict(
                "another switch is already scheduled",
            ));
        }
        if self
            .periods
            .iter()
            .any(|period| period.connection_id == connection_id)
        {
            return Err(AccountingError::Conflict(
                "switch requires a new destination binding",
            ));
        }
        if cutover_at <= now {
            return Err(AccountingError::Invalid("cutover must be in the future"));
        }
        let current = self
            .periods
            .last_mut()
            .ok_or(AccountingError::Conflict("no active destination"))?;
        current.ends_at = Some(cutover_at);
        self.periods.push(DestinationPeriod {
            connection_id,
            starts_at: cutover_at,
            ends_at: None,
        });
        self.revision += 1;
        Ok(())
    }

    pub fn cancel_switch(&mut self, now: DateTime<Utc>) -> Result<(), AccountingError> {
        if self.periods.len() < 2 {
            return Err(AccountingError::Conflict("no switch is scheduled"));
        }
        if self
            .periods
            .last()
            .is_none_or(|period| period.starts_at <= now)
        {
            return Err(AccountingError::Conflict("switch already took effect"));
        }
        self.periods.pop();
        if let Some(last) = self.periods.last_mut() {
            last.ends_at = None;
        }
        self.revision += 1;
        Ok(())
    }

    /// New work is never silently backfilled into a historical destination.
    pub fn destination_for(
        &self,
        accounting_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<ConnectionId, AccountingError> {
        if accounting_at > now {
            return Err(AccountingError::Invalid(
                "posting timestamp cannot be in the future",
            ));
        }
        let period = self
            .periods
            .iter()
            .find(|p| {
                p.starts_at <= accounting_at && p.ends_at.is_none_or(|end| accounting_at < end)
            })
            .ok_or(AccountingError::HistoricalImport)?;
        if period
            .ends_at
            .is_some_and(|ends_at| now >= ends_at + LATE_EVENT_GRACE)
        {
            return Err(AccountingError::HistoricalImport);
        }
        Ok(period.connection_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountingEvent {
    pub book_id: BookId,
    pub revision: i64,
    pub operation: &'static str,
    pub posting_id: Option<PostingId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn at() -> DateTime<Utc> {
        "2026-10-26T00:00:00Z".parse().unwrap()
    }
    fn connection(id: i64, provider: Provider) -> Connection {
        Connection::new(
            ConnectionId::try_from(id).unwrap(),
            provider,
            format!("organization-{id}"),
            AccountMappings {
                revenue_account: "sales".into(),
                refund_account: "returns".into(),
                clearing_account: "cash".into(),
                tax_codes: BTreeMap::from([("standard".into(), "configured-tax".into())]),
            },
        )
        .unwrap()
    }
    fn book() -> Book {
        let mut book = Book::new(
            BookId::try_from(1).unwrap(),
            TenantId::try_from(2).unwrap(),
            "main".into(),
        )
        .unwrap();
        book.add_connection(connection(3, Provider::Freee)).unwrap();
        book.add_connection(connection(4, Provider::Xero)).unwrap();
        book.activate_initial(ConnectionId::try_from(3).unwrap(), at(), at())
            .unwrap();
        book
    }

    fn id(n: i64) -> ConnectionId {
        ConnectionId::try_from(n).unwrap()
    }

    #[test]
    fn switch_takes_effect_at_cutover_without_completion() {
        let mut book = book();
        let cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), cutoff, at()).unwrap();
        assert_eq!(book.active_connection(at()), Some(id(3)));
        assert_eq!(book.active_connection(cutoff), Some(id(4)));
        assert_eq!(book.periods()[0].ends_at, Some(cutoff));
        book.validate().unwrap();
    }

    #[test]
    fn cutover_boundary_is_half_open() {
        let mut book = book();
        let cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), cutoff, at()).unwrap();
        let now = cutoff + TimeDelta::hours(1);
        assert_eq!(
            book.destination_for(cutoff - TimeDelta::microseconds(1), now),
            Ok(id(3))
        );
        assert_eq!(book.destination_for(cutoff, now), Ok(id(4)));
    }

    #[test]
    fn late_event_routes_to_closed_period_inside_grace() {
        let mut book = book();
        let cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), cutoff, at()).unwrap();
        let now = cutoff + TimeDelta::days(30);
        assert_eq!(book.destination_for(at(), now), Ok(id(3)));
        assert_eq!(book.draining_connections(now), vec![id(3)]);
    }

    #[test]
    fn late_grace_boundary_is_exclusive() {
        let mut book = book();
        let cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), cutoff, at()).unwrap();
        let at_grace = cutoff + LATE_EVENT_GRACE;
        assert_eq!(
            book.destination_for(at(), at_grace),
            Err(AccountingError::HistoricalImport)
        );
        assert_eq!(
            book.destination_for(at(), at_grace - TimeDelta::microseconds(1)),
            Ok(id(3))
        );
        assert!(book.draining_connections(at_grace).is_empty());
    }

    #[test]
    fn events_before_first_period_or_in_future_are_rejected() {
        let book = book();
        assert_eq!(
            book.destination_for(at() - TimeDelta::seconds(1), at()),
            Err(AccountingError::HistoricalImport)
        );
        assert!(matches!(
            book.destination_for(at() + TimeDelta::seconds(1), at()),
            Err(AccountingError::Invalid(_))
        ));
    }

    #[test]
    fn only_one_switch_can_be_scheduled() {
        let mut book = book();
        let cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), cutoff, at()).unwrap();
        book.add_connection(connection(5, Provider::MoneyForward))
            .unwrap();
        assert!(matches!(
            book.schedule_switch(id(5), cutoff + TimeDelta::days(1), at()),
            Err(AccountingError::Conflict(_))
        ));
        assert!(
            book.schedule_switch(id(5), cutoff + TimeDelta::days(1), cutoff)
                .is_ok()
        );
    }

    #[test]
    fn cancel_before_cutover_restores_open_period() {
        let mut book = book();
        let cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), cutoff, at()).unwrap();
        book.cancel_switch(at()).unwrap();
        assert_eq!(book.periods().len(), 1);
        assert_eq!(book.periods()[0].ends_at, None);
        book.validate().unwrap();
    }

    #[test]
    fn cancel_at_cutover_instant_conflicts() {
        let mut book = book();
        let cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), cutoff, at()).unwrap();
        let before = book.clone();
        assert_eq!(
            book.cancel_switch(cutoff),
            Err(AccountingError::Conflict("switch already took effect"))
        );
        assert_eq!(book, before);
    }

    #[test]
    fn cancel_without_scheduled_switch_conflicts() {
        let mut book = book();
        assert_eq!(
            book.cancel_switch(at()),
            Err(AccountingError::Conflict("no switch is scheduled"))
        );
    }

    #[test]
    fn activation_backfill_is_bounded() {
        let mut fresh1 = Book::new(
            BookId::try_from(1).unwrap(),
            TenantId::try_from(2).unwrap(),
            "main".into(),
        )
        .unwrap();
        fresh1
            .add_connection(connection(3, Provider::Freee))
            .unwrap();
        assert!(
            fresh1
                .activate_initial(id(3), at() - MAX_BACKFILL, at())
                .is_ok()
        );

        let mut fresh2 = Book::new(
            BookId::try_from(1).unwrap(),
            TenantId::try_from(2).unwrap(),
            "main".into(),
        )
        .unwrap();
        fresh2
            .add_connection(connection(3, Provider::Freee))
            .unwrap();
        assert!(matches!(
            fresh2.activate_initial(
                id(3),
                at() - MAX_BACKFILL - TimeDelta::microseconds(1),
                at()
            ),
            Err(AccountingError::Invalid(_))
        ));
    }

    #[test]
    fn cannot_reschedule_or_reuse_a_historical_binding() {
        let mut book = book();
        let cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), cutoff, at()).unwrap();
        assert!(book.schedule_switch(id(4), cutoff, at()).is_err());
        assert!(
            book.schedule_switch(id(3), cutoff + TimeDelta::days(1), cutoff)
                .is_err()
        );
    }

    #[test]
    fn missing_tax_mappings_and_duplicate_bindings_are_rejected() {
        let mut candidate = connection(5, Provider::Freee);
        candidate.mappings.tax_codes.clear();
        assert!(candidate.mappings.validate().is_err());
        let mut book = book();
        assert!(book.add_connection(connection(3, Provider::Freee)).is_err());
    }

    #[test]
    fn forged_persisted_periods_fail_closed() {
        let mut book = book();
        book.periods[0].ends_at = Some(at());
        assert!(book.validate().is_err());
    }

    #[test]
    fn returning_to_a_previous_provider_uses_a_fresh_binding() {
        let mut book = book();
        let first_cutoff = at() + TimeDelta::days(1);
        book.schedule_switch(id(4), first_cutoff, at()).unwrap();
        let mut returning = connection(5, Provider::Freee);
        returning.organization_id = "organization-3".into();
        book.add_connection(returning).unwrap();
        let next_cutoff = first_cutoff + TimeDelta::days(1);
        book.schedule_switch(id(5), next_cutoff, first_cutoff)
            .unwrap();
        assert_eq!(book.active_connection(next_cutoff), Some(id(5)));
        assert_eq!(book.periods()[0].connection_id, id(3));
        book.validate().unwrap();
    }
}
