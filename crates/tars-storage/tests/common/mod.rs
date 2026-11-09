//! The claim a task is held by, for tests that end a task as the runner holding it.

use tars_storage::durable::{DurableStoreRead, Writer};
use tars_types::TaskId;

/// Returns the claim holding task `id` now. It panics on a task nobody holds, so a test
/// cannot quietly become an unclaimed writer.
pub async fn holder<B: DurableStoreRead + ?Sized>(b: &B, id: &TaskId) -> Writer {
    let token = b
        .get(id)
        .await
        .expect("a test durable store reads")
        .and_then(|t| t.lease)
        .map(|l| l.token)
        .unwrap_or_else(|| panic!("task `{id}` is held by no claim"));
    Writer::Claim(token)
}
