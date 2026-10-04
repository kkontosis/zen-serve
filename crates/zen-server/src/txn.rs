//! The transaction retry loop.

/// Run `$body` in a transaction, retrying on conflict when the server chose
/// the read version (`$rv` is `None`). Evaluates to
/// `ApiResult<(body value, commit version)>`. Inside `$body`, `$t` is the
/// `Box<dyn Txn>`, and `?` aborts the attempt.
macro_rules! txn_loop {
    ($store:expr, $rv:expr, |$t:ident| $body:block) => {{
        let rv: Option<zen_store::Version> = $rv;
        let mut attempt = 0u32;
        loop {
            let mut $t = match $store.begin(rv).await {
                Ok(t) => t,
                Err(e) => break Err($crate::error::ApiError::from(e)),
            };
            let r: $crate::error::ApiResult<_> = async { $body }.await;
            let v = match r {
                Ok(v) => v,
                Err(e) => break Err(e),
            };
            match $t.commit().await {
                Ok(ver) => break Ok((v, ver)),
                Err(zen_store::Error::Conflict) if rv.is_none() && attempt < 20 => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        (1u64 << attempt.min(6)) / 2,
                    ))
                    .await;
                }
                Err(e) => break Err($crate::error::ApiError::from(e)),
            }
        }
    }};
}

pub(crate) use txn_loop;
