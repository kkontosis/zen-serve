//! The signed ACL (spec/formats.md §9, api.md §4.1–4.2): fetched and walked
//! from the pinned version, so a server can't serve an older or another one.

use crate::error::{Error, Result};
use crate::session::Session;
use zen_core::kdf::acl_hash;
use zen_core::labels;
use zen_core::sig::{PublicIdentity, SigningIdentity};
use zen_proto::acl::{AclDoc, Member, SignedAcl};
use zen_proto::{AclEntries, AclGet, AclPut, from_cbor, to_cbor};

/// A verified ACL version.
#[derive(Debug, Clone)]
pub struct AclVersion {
    /// The version.
    pub version: u64,
    /// The document.
    pub doc: AclDoc,
    /// The signed document bytes, as received (hashing and signing use these).
    pub doc_bytes: Vec<u8>,
    /// The signed form.
    pub signed: SignedAcl,
    /// The whole signed version, as received.
    pub bytes: Vec<u8>,
}

impl AclVersion {
    /// Parse a signed ACL version (no chain check).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let signed: SignedAcl =
            from_cbor(bytes).map_err(|e| Error::format(format!("signed ACL: {e}")))?;
        let doc: AclDoc =
            from_cbor(&signed.doc).map_err(|e| Error::format(format!("ACL doc: {e}")))?;
        Ok(AclVersion {
            version: doc.version,
            doc_bytes: signed.doc.clone(),
            doc,
            signed,
            bytes: bytes.to_vec(),
        })
    }

    /// `H(doc)` (formats.md §9.2).
    pub fn hash(&self) -> [u8; 32] {
        acl_hash(&self.doc_bytes)
    }

    /// The member with this user fingerprint.
    pub fn member(&self, user_fp: &[u8]) -> Option<&Member> {
        self.doc
            .members
            .iter()
            .find(|m| PublicIdentity::decode(&m.identity).is_ok_and(|p| p.fingerprint() == user_fp))
    }

    /// The member identity of an admin, by user fingerprint.
    pub fn admin(&self, user_fp: &[u8]) -> Option<PublicIdentity> {
        if !self.doc.admins.iter().any(|a| a.as_slice() == user_fp) {
            return None;
        }
        PublicIdentity::decode(&self.member(user_fp)?.identity).ok()
    }
}

fn chain_error(msg: impl Into<String>) -> Error {
    Error::local("acl_chain", msg)
}

/// Check `next` against `prev` (formats.md §9.3 rules 1–2, and the
/// signature): the next version, naming the previous one's hash, signed by
/// one of its admins (by `next`'s own for version 1).
pub fn verify_next(prev: Option<&AclVersion>, next: &AclVersion) -> Result<()> {
    let want = prev.map_or(1, |p| p.version + 1);
    if next.version != want {
        return Err(chain_error(format!(
            "version {}, expected {want}",
            next.version
        )));
    }
    let prev_hash = prev.map_or([0; 32], AclVersion::hash);
    if next.doc.prev_hash != prev_hash {
        return Err(chain_error("prev_hash mismatch"));
    }
    let authority = prev.unwrap_or(next);
    let signer = &next.signed.signer;
    if !authority
        .doc
        .admins
        .iter()
        .any(|a| a.as_slice() == signer.as_slice())
    {
        return Err(chain_error("signer is not an admin"));
    }
    let m = authority
        .member(signer)
        .ok_or_else(|| chain_error("signer is not a member"))?;
    PublicIdentity::decode(&m.identity)
        .and_then(|p| p.verify(labels::SIG_ACL, &next.doc_bytes, &next.signed.sig))
        .map_err(|_| chain_error(format!("bad signature on version {}", next.version)))
}

/// Sign `doc` with `signer`: the signed ACL bytes.
pub fn sign_doc(doc: &AclDoc, signer: &SigningIdentity) -> Result<Vec<u8>> {
    let doc_bytes = to_cbor(doc);
    let sig = signer.sign(labels::SIG_ACL, &doc_bytes)?;
    Ok(to_cbor(&SignedAcl {
        doc: doc_bytes,
        sig,
        signer: signer.public().fingerprint().to_vec(),
    }))
}

/// The ACL as one session sees it. Every handle of a session shares its pin.
pub struct Acl {
    session: Session,
}

impl Acl {
    pub(crate) fn new(session: Session) -> Self {
        Acl { session }
    }

    /// The signed versions from `from` (default: the head) up to the head.
    pub async fn entries(&self, from: Option<u64>) -> Result<AclEntries> {
        self.session.call("/v1/acl/get", &AclGet { from }).await
    }

    /// Fetch and verify the chain from the pinned version (from version 1
    /// the first time), and return the head. A head below the pin, or a
    /// pinned version that changed or is gone, is `acl_chain`.
    pub async fn head(&self) -> Result<AclVersion> {
        let mut prev = self.pinned();
        let from = prev.as_ref().map_or(1, |p| p.version);
        let r = match self.entries(Some(from)).await {
            Err(e) if prev.is_some() && e.is("not_found") => {
                return Err(chain_error(format!(
                    "the served ACL is below the pinned version {from}"
                )));
            }
            r => r?,
        };
        if let Some(p) = &prev
            && r.head < p.version
        {
            return Err(chain_error(format!(
                "the served head {} is below the pinned version {}",
                r.head, p.version
            )));
        }
        for bytes in &r.entries {
            let v = AclVersion::parse(bytes)?;
            if let Some(p) = &prev
                && v.version == p.version
            {
                if v.doc_bytes != p.doc_bytes {
                    return Err(chain_error("the pinned version changed"));
                }
                continue;
            }
            verify_next(prev.as_ref(), &v)?;
            prev = Some(v);
        }
        let head = prev.ok_or_else(|| chain_error("no ACL"))?;
        let mut slot = self.session.acl_slot().lock().expect("acl");
        if slot.as_ref().is_none_or(|p| head.version >= p.version) {
            *slot = Some(head.clone());
        }
        Ok(head)
    }

    /// The newest verified version, if any (no request).
    pub fn pinned(&self) -> Option<AclVersion> {
        self.session.acl_slot().lock().expect("acl").clone()
    }

    /// Pin a version kept from an earlier process, so `head` must extend it.
    /// A version below the current pin is ignored.
    pub fn pin(&self, bytes: &[u8]) -> Result<()> {
        let v = AclVersion::parse(bytes)?;
        let mut slot = self.session.acl_slot().lock().expect("acl");
        if slot.as_ref().is_none_or(|p| v.version > p.version) {
            *slot = Some(v);
        }
        Ok(())
    }

    /// Change the ACL: `edit` gets a copy of the head document, and the
    /// result is signed by `signer` (an admin of the head) as the next
    /// version. Retries a concurrent change (409 `version_mismatch`) twice.
    pub async fn update(
        &self,
        signer: &SigningIdentity,
        mut edit: impl FnMut(&mut AclDoc),
    ) -> Result<AclVersion> {
        for i in 1.. {
            let head = self.head().await?;
            let mut doc = head.doc.clone();
            edit(&mut doc);
            doc.version = head.version + 1;
            doc.prev_hash = head.hash().to_vec();
            let put = AclPut {
                acl: sign_doc(&doc, signer)?,
                claim: None,
                origin: None,
            };
            match self
                .session
                .call::<_, zen_proto::AclVersion>("/v1/acl/put", &put)
                .await
            {
                Ok(_) => return self.head().await,
                Err(e) if e.is("version_mismatch") && i < 3 => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
}
