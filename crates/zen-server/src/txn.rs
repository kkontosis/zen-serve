//! The transaction retry loop.

/// Run `$body` in a transaction, retrying on conflict and retryable storage
/// errors when the server chose the read version (`$rv` is `None`).
/// Evaluates to `ApiResult<(body value, commit versionstamp)>`. Inside
/// `$body`, `$t` is the `Box<dyn Txn>`, and `?` aborts the attempt.
///
/// An unknown commit result is retried only in the `idempotent` form, whose
/// body must detect its own earlier commit (`/v1/commit` reads its
/// `commit_id` record first). Otherwise it surfaces as `commit_unknown`.
macro_rules! txn_loop {
    ($store:expr, $rv:expr, |$t:ident| $body:block) => {
        $crate::txn::txn_loop!(@run $store, $rv, false, |$t| $body)
    };
    ($store:expr, $rv:expr, idempotent, |$t:ident| $body:block) => {
        $crate::txn::txn_loop!(@run $store, $rv, true, |$t| $body)
    };
    (@run $store:expr, $rv:expr, $idem:expr, |$t:ident| $body:block) => {{
        let rv: Option<zen_store::Version> = $rv;
        let mut attempt = 0u32;
        loop {
            let retry = |e: &zen_store::Error| match e {
                zen_store::Error::Conflict | zen_store::Error::TooOld => rv.is_none(),
                zen_store::Error::CommitUnknown => $idem,
                _ => false,
            };
            let mut $t = match $store.begin(rv).await {
                Ok(t) => t,
                Err(e) if retry(&e) && attempt < 20 => {
                    attempt += 1;
                    $crate::txn::backoff(attempt).await;
                    continue;
                }
                Err(e) => break Err($crate::error::ApiError::from(e)),
            };
            let r: $crate::error::ApiResult<_> = async { $body }.await;
            let v = match r {
                Ok(v) => v,
                // A read hit a retryable storage error (only storage errors
                // carry `too_old`).
                Err(e) if e.code == "too_old" && rv.is_none() && attempt < 20 => {
                    attempt += 1;
                    $crate::txn::backoff(attempt).await;
                    continue;
                }
                Err(e) => break Err(e),
            };
            match $t.commit().await {
                Ok(stamp) => break Ok((v, stamp)),
                Err(e) if retry(&e) && attempt < 20 => {
                    attempt += 1;
                    $crate::txn::backoff(attempt).await;
                }
                Err(e) => break Err($crate::error::ApiError::from(e)),
            }
        }
    }};
}

pub(crate) use txn_loop;

/// Exponential backoff before retry `attempt` (1-based): 1 ms … 32 ms.
pub(crate) async fn backoff(attempt: u32) {
    tokio::time::sleep(std::time::Duration::from_millis(
        (1u64 << attempt.min(6)) / 2,
    ))
    .await;
}
