//! Records facts about the durable store itself in `store_meta`, written by the runner that first
//! opens it: the tenant whose work the durable store holds, and the durable store's id. The two
//! are one row, so a store records both or neither.
use std::str::FromStr;

use sqlx::{SqliteConnection, SqlitePool};
use tars_types::{DurableStoreId, TenantId};
use uuid::Uuid;

use crate::durable::DurableStoreInitError;

/// Whose work a durable store holds and which store it is, as the runner that claimed it
/// recorded them.
pub(super) struct StoreIdentity {
    pub(super) tenant: TenantId,
    pub(super) id: DurableStoreId,
}

/// Records `given` as the durable store's tenant, with a fresh durable store id, on its first
/// open; on every later open, refuses a different tenant and returns the id already there. One
/// `IMMEDIATE` transaction, so of two first opens racing with different tenants the first to
/// commit wins and the other is refused.
pub(super) async fn claim(
    pool: &SqlitePool,
    given: &TenantId,
) -> Result<DurableStoreId, DurableStoreInitError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("INSERT OR IGNORE INTO store_meta (one, tenant, store_id) VALUES (1, ?, ?)")
        .bind(given.as_str())
        .bind(Uuid::new_v4().to_string())
        .execute(&mut *tx)
        .await?;
    let (tenant, store_id): (String, String) =
        sqlx::query_as("SELECT tenant, store_id FROM store_meta")
            .fetch_one(&mut *tx)
            .await?;
    let stored = StoreIdentity::decoded(tenant, store_id)?;
    tx.commit().await?;

    if stored.tenant != *given {
        return Err(DurableStoreInitError::TenantMismatch {
            stored: stored.tenant,
            given: given.clone(),
        });
    }
    Ok(stored.id)
}

/// Replaces the tenant the durable store records with `to` when it records one of `from`, for a
/// caller that changed how it names its tenants. Returns the tenant it replaced, or `None`
/// when it wrote nothing: the durable store is unclaimed (the next open claims it for `to`) or
/// already `to`'s. A durable store another tenant holds is refused with
/// [`DurableStoreInitError::TenantMismatch`] and left as it was. Only the tenant changes; the
/// durable store id and the work stay. One `IMMEDIATE` transaction, so a first open racing it sees
/// the durable store either before or after.
pub async fn retenant(
    pool: &SqlitePool,
    from: &[TenantId],
    to: &TenantId,
) -> Result<Option<TenantId>, DurableStoreInitError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let was = moved(tenant_in(&mut tx).await?, from, to)?;
    if was.is_some() {
        sqlx::query("UPDATE store_meta SET tenant = ?")
            .bind(to.as_str())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(was)
}

/// Returns the tenant `retenant` replaces given what the durable store records, `None` when there
/// is nothing to change.
fn moved(
    recorded: Option<TenantId>,
    from: &[TenantId],
    to: &TenantId,
) -> Result<Option<TenantId>, DurableStoreInitError> {
    match recorded {
        None => Ok(None),
        Some(stored) if stored == *to => Ok(None),
        Some(stored) if from.contains(&stored) => Ok(Some(stored)),
        Some(stored) => Err(DurableStoreInitError::TenantMismatch {
            stored,
            given: to.clone(),
        }),
    }
}

/// Returns the tenant recorded on the durable store behind `pool`, or `None` when no runner has
/// claimed it. It only reads, so a read-only pool on someone's live durable store will do.
pub async fn recorded_tenant(pool: &SqlitePool) -> Result<Option<TenantId>, sqlx::Error> {
    tenant_in(&mut *pool.acquire().await?).await
}

/// Returns the tenant and id recorded on the durable store behind `pool`, or `None` when no
/// runner has claimed it.
pub(super) async fn recorded_identity(
    pool: &SqlitePool,
) -> Result<Option<StoreIdentity>, sqlx::Error> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT tenant, store_id FROM store_meta")
        .fetch_optional(pool)
        .await?;
    row.map(|(tenant, store_id)| StoreIdentity::decoded(tenant, store_id))
        .transpose()
}

async fn tenant_in(conn: &mut SqliteConnection) -> Result<Option<TenantId>, sqlx::Error> {
    let tenant: Option<String> = sqlx::query_scalar("SELECT tenant FROM store_meta")
        .fetch_optional(conn)
        .await?;
    tenant.map(decoded).transpose()
}

impl StoreIdentity {
    fn decoded(tenant: String, store_id: String) -> Result<Self, sqlx::Error> {
        Ok(Self {
            tenant: decoded(tenant)?,
            id: decoded(store_id)?,
        })
    }
}

/// Parses a stored value into its id; a value the id refuses is a decode error.
fn decoded<T>(text: String) -> Result<T, sqlx::Error>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    text.parse()
        .map_err(|e: T::Err| sqlx::Error::Decode(Box::new(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_empty_tenant_is_a_decode_error() {
        assert!(matches!(
            decoded::<TenantId>(String::new()),
            Err(sqlx::Error::Decode(_))
        ));
        assert_eq!(decoded::<TenantId>("t".to_owned()).ok(), Some(t("t")));
    }

    fn t(name: &str) -> TenantId {
        name.parse::<TenantId>().unwrap()
    }

    #[test]
    fn an_unclaimed_store_is_left_for_the_next_open_to_claim() {
        assert!(matches!(moved(None, &[t("o/old")], &t("id")), Ok(None)));
    }

    #[test]
    fn a_store_already_the_new_tenants_is_left_as_it_is() {
        assert!(matches!(
            moved(Some(t("id")), &[t("o/old")], &t("id")),
            Ok(None)
        ));
    }

    #[test]
    fn a_store_holding_an_old_value_moves_and_names_what_it_held() {
        let got = moved(Some(t("o/old")), &[t("o/new"), t("o/old")], &t("id"));
        assert_eq!(got.ok().flatten(), Some(t("o/old")));
    }

    #[test]
    fn a_store_holding_a_value_not_given_is_refused_with_both() {
        match moved(Some(t("o/other")), &[t("o/old")], &t("id")) {
            Err(DurableStoreInitError::TenantMismatch { stored, given }) => {
                assert_eq!((stored, given), (t("o/other"), t("id")));
            }
            other => panic!("expected TenantMismatch, got {other:?}"),
        }
    }

    #[test]
    fn the_new_tenant_among_the_old_values_is_still_no_change() {
        assert!(matches!(
            moved(Some(t("id")), &[t("id")], &t("id")),
            Ok(None)
        ));
    }
}
