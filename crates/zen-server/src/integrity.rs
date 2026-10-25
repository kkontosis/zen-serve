//! Device checkpoints and the device registry (api.md §15, keyspace.md
//! §3.10).
//!
//! The server stores each device's sealed, signed checkpoints per fs and
//! can't read them. It enforces only what keeps the log well formed: one
//! put per `seq`, in order, by a device of the caller's user. Verifying
//! them is the clients' job (formats.md §14.4).

use crate::acl::Fp;
use crate::auth::Caller;
use crate::cbor::Cbor;
use crate::error::*;
use crate::keys;
use crate::state::Shared;
use crate::txn::txn_loop;
use axum::extract::State;
use zen_core::seal::{Kind, peek};
use zen_core::sig::verify_device_cert;
use zen_proto::{
    CheckpointGet, CheckpointHead, CheckpointHeadList, CheckpointHeads, CheckpointItem,
    CheckpointList, CheckpointPut, CheckpointVersion, DeviceInfo, DeviceList, DeviceRegister,
    DeviceRegistered, DevicesGet,
};
use zen_store::tuple::{Elem, unpack_prefix};
use zen_store::{Txn, key_after};

/// Devices a user may register (api.md §15.2).
pub const MAX_USER_DEVICES: usize = 64;

/// Devices per `/v1/devices/get`.
const MAX_GET_DEVICES: usize = 1000;

/// 403 unless the caller holds a right on `fs`: an fs right or a topic
/// grant (api.md §15.1).
pub fn require_any_right(st: &Shared, caller: &Caller, fs: u32) -> ApiResult<()> {
    st.check_fs(fs)?;
    if caller.acl.fs_rights(&caller.user, fs) != 0 || caller.acl.has_topic_grants(&caller.user, fs)
    {
        Ok(())
    } else {
        Err(forbidden("no right on this fs"))
    }
}

fn fp(b: &[u8], what: &str) -> ApiResult<Fp> {
    b.try_into()
        .map_err(|_| bad_request(format!("{what} must be 32 bytes")))
}

fn decode_seq(v: &[u8]) -> ApiResult<u64> {
    Ok(u64::from_be_bytes(
        v.try_into()
            .map_err(|_| internal("corrupt checkpoint head"))?,
    ))
}

/// The registered device `dev`: `(user, cert)`.
async fn registered(t: &mut Box<dyn Txn>, dev: &[u8]) -> ApiResult<Option<(Fp, Vec<u8>)>> {
    Ok(match t.get(&keys::device(dev)).await? {
        Some(v) if v.len() > 32 => Some((v[..32].try_into().expect("32"), v[32..].to_vec())),
        Some(_) => return Err(internal("corrupt device record")),
        None => None,
    })
}

/// Every device's head in `fs`, in device order (at most `max`).
pub async fn read_heads(t: &mut Box<dyn Txn>, fs: u32, max: usize) -> ApiResult<Vec<(Fp, u64)>> {
    let pfx = keys::ckpt_heads(fs).finish();
    let got = t
        .snapshot_get_range(&pfx, &keys::end_of(&pfx), max, false)
        .await?;
    let mut out = Vec::with_capacity(got.len());
    for (k, v) in got {
        let (elems, _) =
            unpack_prefix(&k[pfx.len()..], 1).map_err(|_| internal("bad checkpoint head key"))?;
        let [Elem::Bytes(d)] = elems.as_slice() else {
            return Err(internal("bad checkpoint head key"));
        };
        out.push((fp(d, "device")?, decode_seq(&v)?));
    }
    Ok(out)
}

/// `POST /v1/checkpoint/put`.
pub async fn put(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<CheckpointPut>,
) -> ApiResult<Cbor<CheckpointVersion>> {
    require_any_right(&st, &caller, req.fs)?;
    let dev = fp(&req.device, "device")?;
    if req.checkpoint.len() > st.cfg.limits.max_value_bytes as usize {
        return Err(too_large("checkpoint too large"));
    }
    if !matches!(peek(&req.checkpoint), Ok((Kind::Checkpoint, _))) {
        return Err(bad_request("not a sealed checkpoint (kind 11)"));
    }
    if req.seq == 0 {
        return Err(bad_request("seq starts at 1"));
    }
    let keep = u64::from(st.cfg.limits.checkpoint_keep);
    let limits = caller.acl.limits.clone();
    // Idempotent: a put resent after an unknown result finds its own
    // checkpoint at the head and answers with the stored version.
    let (stored, stamp) = txn_loop!(st.store, None, idempotent, |t| {
        if !caller.acl.has_device(&caller.user, &dev) {
            match registered(&mut t, &dev).await? {
                Some((user, _)) if user == caller.user => {}
                _ => return Err(forbidden("the device is not one of the caller's")),
            }
        }
        let head = match t.get(&keys::ckpt_head(req.fs, &dev)).await? {
            Some(v) => decode_seq(&v)?,
            None => 0,
        };
        if req.seq == head
            && let Some(v) = t.get(&keys::ckpt(req.fs, &dev, req.seq)).await?
            && v.len() >= 10
            && v[10..] == req.checkpoint[..]
        {
            return Ok(Some(v[..10].to_vec()));
        }
        if req.seq != head + 1 {
            return Err(version_mismatch(format!(
                "the device's head is {head}: the next checkpoint is {}",
                head + 1
            )));
        }
        let mut delta = (req.checkpoint.len() as i64, 1i64);
        if req.seq > keep {
            let old = keys::ckpt(req.fs, &dev, req.seq - keep);
            if let Some(v) = t.snapshot_get(&old).await? {
                delta.0 -= v.len().saturating_sub(10) as i64;
                delta.1 -= 1;
                t.clear(&old);
            }
        }
        crate::quota::apply(&mut t, &limits, &[(req.fs, delta)].into()).await?;
        t.set_versionstamped_value(&keys::ckpt(req.fs, &dev, req.seq), &[], &req.checkpoint);
        t.set(&keys::ckpt_head(req.fs, &dev), &req.seq.to_be_bytes());
        t.set_versionstamped_value(&keys::ckpt_bell(req.fs), &[], &[]);
        Ok(None)
    })?;
    Ok(Cbor(CheckpointVersion {
        version: stored.unwrap_or_else(|| stamp.to_vec()),
    }))
}

/// `POST /v1/checkpoint/heads`.
pub async fn heads(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<CheckpointHeads>,
) -> ApiResult<Cbor<CheckpointHeadList>> {
    require_any_right(&st, &caller, req.fs)?;
    let max = st.cfg.limits.max_range_items as usize;
    let mut t = st.view.begin(None).await?;
    let heads = match &req.devices {
        None => read_heads(&mut t, req.fs, max).await?,
        Some(devs) => {
            if devs.len() > max {
                return Err(too_large("too many devices"));
            }
            let mut want: Vec<Fp> = devs
                .iter()
                .map(|d| fp(d, "device"))
                .collect::<ApiResult<_>>()?;
            want.sort();
            want.dedup();
            let mut out = Vec::new();
            for d in want {
                if let Some(v) = t.snapshot_get(&keys::ckpt_head(req.fs, &d)).await? {
                    out.push((d, decode_seq(&v)?));
                }
            }
            out
        }
    };
    Ok(Cbor(CheckpointHeadList {
        heads: heads
            .into_iter()
            .map(|(device, seq)| CheckpointHead {
                device: device.to_vec(),
                seq,
            })
            .collect(),
    }))
}

/// `POST /v1/checkpoint/get`.
pub async fn get(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<CheckpointGet>,
) -> ApiResult<Cbor<CheckpointList>> {
    require_any_right(&st, &caller, req.fs)?;
    let dev = fp(&req.device, "device")?;
    let l = &st.cfg.limits;
    let limit = req
        .limit
        .unwrap_or(l.max_range_items)
        .clamp(1, l.max_range_items) as usize;
    let begin = keys::ckpt(req.fs, &dev, req.from);
    let end = match req.to {
        Some(to) if to < i64::MAX as u64 => key_after(&keys::ckpt(req.fs, &dev, to)),
        _ => keys::end_of(&keys::ckpts(req.fs, &dev).finish()),
    };
    let mut items = Vec::new();
    // Sequence numbers are tuple integers (i64): larger ones hold nothing.
    if req.from <= i64::MAX as u64 && begin < end {
        let mut t = st.view.begin(None).await?;
        let (got, capped) = crate::kv::read_capped(
            &mut t,
            &begin,
            &end,
            limit + 1,
            false,
            l.max_range_bytes as usize,
            false,
        )
        .await?;
        let more = capped || got.len() > limit;
        let pfx = keys::ckpts(req.fs, &dev).finish();
        for (k, mut v) in got.into_iter().take(limit) {
            let (elems, _) =
                unpack_prefix(&k[pfx.len()..], 1).map_err(|_| internal("bad checkpoint key"))?;
            let [Elem::Int(seq)] = elems.as_slice() else {
                return Err(internal("bad checkpoint key"));
            };
            if v.len() < 10 {
                return Err(internal("corrupt checkpoint"));
            }
            items.push(CheckpointItem {
                seq: *seq as u64,
                checkpoint: v.split_off(10),
            });
        }
        return Ok(Cbor(CheckpointList { items, more }));
    }
    Ok(Cbor(CheckpointList { items, more: false }))
}

/// `POST /v1/devices/register`.
pub async fn register(
    State(st): State<Shared>,
    caller: Caller,
    Cbor(req): Cbor<DeviceRegister>,
) -> ApiResult<Cbor<DeviceRegistered>> {
    let member = caller
        .acl
        .members
        .get(&caller.user)
        .ok_or_else(|| forbidden("not a member"))?;
    let (device, _) = verify_device_cert(&member.identity, &req.cert)
        .map_err(|_| bad_request("the certificate doesn't verify against the caller's identity"))?;
    let dev = device.fingerprint();
    let mut record = caller.user.to_vec();
    record.extend_from_slice(&req.cert);
    // Idempotent: registering again is a no-op, so a resend after an
    // unknown result finds the device registered.
    txn_loop!(st.store, None, idempotent, |t| {
        match registered(&mut t, &dev).await? {
            Some((user, _)) if user == caller.user => return Ok(()),
            Some(_) => return Err(conflict("the device belongs to another user")),
            None => {}
        }
        let pfx = keys::user_devices(&caller.user).finish();
        let n = t
            .get_range(&pfx, &keys::end_of(&pfx), MAX_USER_DEVICES, false)
            .await?
            .len();
        if n >= MAX_USER_DEVICES {
            return Err(quota(format!(
                "a user registers at most {MAX_USER_DEVICES} devices"
            )));
        }
        t.set(&keys::device(&dev), &record);
        t.set(&keys::user_devices(&caller.user).bytes(&dev).finish(), &[]);
        Ok(())
    })?;
    Ok(Cbor(DeviceRegistered {
        device: dev.to_vec(),
    }))
}

/// `POST /v1/devices/get`.
pub async fn devices_get(
    State(st): State<Shared>,
    _caller: Caller,
    Cbor(req): Cbor<DevicesGet>,
) -> ApiResult<Cbor<DeviceList>> {
    if req.devices.len() > MAX_GET_DEVICES {
        return Err(too_large("too many devices"));
    }
    let mut t = st.view.begin(None).await?;
    let mut devices = Vec::new();
    for d in &req.devices {
        let d = fp(d, "device")?;
        if let Some((user, cert)) = registered(&mut t, &d).await? {
            devices.push(DeviceInfo {
                device: d.to_vec(),
                user: user.to_vec(),
                cert,
            });
        }
    }
    Ok(Cbor(DeviceList { devices }))
}
