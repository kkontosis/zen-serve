//! Hierarchical PRF tokens (spec/formats.md §3).
//!
//! A logical tuple `(e1, …, en)` becomes `t1 || … || tn`, 16 bytes per element:
//!
//! ```text
//! t_i = PRF16(N_{i-1}, lp(e_i))
//! N_i = KDF("zen/v1/name-chain", N_{i-1}, lp(e_i))
//! ```
//!
//! The token of a tuple is a prefix of the token of every extension of it, so
//! the server can range-scan and enforce prefix ACLs without learning names.

use crate::encoding::lp;
use crate::kdf::{Key32, kdf, prf16};
use crate::keys::FsKeys;
use crate::labels;

/// Length of one token element.
pub const ELEMENT_LEN: usize = 16;

/// A position in a naming chain: the accumulated token and the key for the next step.
#[derive(Clone)]
pub struct NameChain {
    token: Vec<u8>,
    key: Key32,
}

impl NameChain {
    /// Start a chain from a root naming key.
    pub fn new(root: Key32) -> Self {
        NameChain {
            token: Vec::new(),
            key: root,
        }
    }

    /// Extend the chain by one element.
    pub fn child(&self, element: &[u8]) -> NameChain {
        let mut enc = Vec::with_capacity(4 + element.len());
        lp(&mut enc, element);
        let mut token = self.token.clone();
        token.extend_from_slice(&prf16(&self.key, &enc));
        NameChain {
            token,
            key: kdf(labels::NAME_CHAIN, &self.key, &enc),
        }
    }

    /// Extend by several elements.
    pub fn path<E: AsRef<[u8]>>(&self, elements: &[E]) -> NameChain {
        elements
            .iter()
            .fold(self.clone(), |c, e| c.child(e.as_ref()))
    }

    /// The accumulated token (16 bytes per element).
    pub fn token(&self) -> &[u8] {
        &self.token
    }

    /// The chain key at this position. Holding it allows deriving every
    /// descendant token, but nothing above or beside it.
    pub fn key(&self) -> &Key32 {
        &self.key
    }
}

/// The stored (server-visible) key for a logical KV tuple in `fs`.
pub fn kv_key<E: AsRef<[u8]>>(fs: &FsKeys, elements: &[E]) -> Vec<u8> {
    NameChain::new(fs.kv_name_root())
        .path(elements)
        .token()
        .to_vec()
}

/// Keys for one topic: its server-visible id and the secrets beneath it.
pub struct TopicKeys {
    name: NameChain,
    data: Key32,
}

impl TopicKeys {
    /// Derive the keys of a topic path (e.g. `["chat", "alice-bob"]`) at the fs's current epoch.
    pub fn new<E: AsRef<[u8]>>(fs: &FsKeys, segments: &[E]) -> Self {
        let mut topic = TopicKeys {
            name: NameChain::new(fs.topic_name_root()),
            data: fs.topic_data_root(),
        };
        for s in segments {
            topic = topic.child(s.as_ref());
        }
        topic
    }

    /// Descend to a sub-topic. Holding a `TopicKeys` delegates its whole subtree.
    pub fn child(&self, segment: &[u8]) -> Self {
        let mut enc = Vec::with_capacity(4 + segment.len());
        lp(&mut enc, segment);
        TopicKeys {
            name: self.name.child(segment),
            data: kdf(labels::TOPIC_DATA_CHAIN, &self.data, &enc),
        }
    }

    /// The topic id the server sees (16 bytes per segment).
    pub fn id(&self) -> &[u8] {
        self.name.token()
    }

    /// `PRF16(KDF("zen/v1/event-key", N_topic, ""), lp(key))`: per-topic event key token.
    pub fn event_key_token(&self, key: &[u8]) -> [u8; 16] {
        let k = kdf(labels::EVENT_KEY, self.name.key(), &[]);
        let mut enc = Vec::with_capacity(4 + key.len());
        lp(&mut enc, key);
        prf16(&k, &enc)
    }

    /// `KDF("zen/v1/event-aead", D_topic, "")`: the AEAD key for events on this topic.
    pub fn event_aead_key(&self) -> Key32 {
        kdf(labels::EVENT_AEAD, &self.data, &[])
    }
}
