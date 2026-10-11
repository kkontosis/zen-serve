//! Paged file versions and file keys (formats.md §11.3–11.6).

use zen_core::Error;
use zen_core::fs::{self, AnyManifest, Body, FileKey, Manifest, ManifestV2};
use zen_core::keys::FsKeys;
use zen_core::rng::{DetRng, OsRng};

fn keys() -> FsKeys {
    FsKeys::generate(7, &mut DetRng::new("fs")).expect("det rng")
}

fn id(b: u8) -> [u8; 16] {
    [b; 16]
}

fn paged(n: u64, page_size: u32) -> ManifestV2 {
    let m = n.div_ceil(page_size as u64);
    ManifestV2 {
        size: n * 10,
        chunk_size: 10,
        n,
        file_key: Some(FileKey {
            key_epoch: 3,
            key: [9; 32],
        }),
        body: Body::Paged {
            page_size,
            pages: (0..m).map(|i| (id(i as u8), [i as u8; 32])).collect(),
        },
    }
}

#[test]
fn manifest_v2_round_trips() {
    let inline = ManifestV2 {
        size: 25,
        chunk_size: 10,
        n: 3,
        file_key: Some(FileKey {
            key_epoch: 1,
            key: [1; 32],
        }),
        body: Body::Inline(vec![id(1), id(2), id(3)]),
    };
    // key_mode 0: no file key, the chunks under the fs keys.
    let fs_keys = ManifestV2 {
        file_key: None,
        ..paged(10, 4)
    };
    assert_eq!(fs_keys.encode().len() + 36, paged(10, 4).encode().len());
    for m in [inline, paged(10, 4), paged(8, 4), paged(0, 4), fs_keys] {
        assert_eq!(ManifestV2::decode(&m.encode()).unwrap(), m);
        let any = AnyManifest::V2(m.clone());
        assert_eq!(AnyManifest::decode(&any.encode()).unwrap(), any);
    }
    let v1 = AnyManifest::V1(Manifest {
        size: 5,
        chunk_size: 10,
        chunks: vec![id(4)],
    });
    assert_eq!(AnyManifest::decode(&v1.encode()).unwrap(), v1);
    // The last page holds what is left.
    let m = paged(10, 4);
    assert_eq!(
        (0..4).map(|i| m.page_len(i)).collect::<Vec<_>>(),
        [Some(4), Some(4), Some(2), None]
    );
}

#[test]
fn manifest_v2_refuses_malformed() {
    // A page count that doesn't match n.
    let mut m = paged(10, 4);
    if let Body::Paged { pages, .. } = &mut m.body {
        pages.pop();
    }
    assert_eq!(ManifestV2::decode(&m.encode()), Err(Error::Format));
    // page_size 0 with chunks.
    let mut b = paged(0, 4).encode();
    b[13..21].copy_from_slice(&1u64.to_be_bytes());
    let form = 1 + 8 + 4 + 8 + 1 + 4 + 32;
    b[form + 1..form + 5].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(ManifestV2::decode(&b), Err(Error::Format));
    // An unknown key mode.
    let mut b = paged(8, 4).encode();
    b[21] = 2;
    assert_eq!(ManifestV2::decode(&b), Err(Error::Format));
    // An unknown form, a short inline list, trailing bytes, another version.
    let mut b = paged(8, 4).encode();
    b[form] = 3;
    assert_eq!(ManifestV2::decode(&b), Err(Error::Format));
    let inline = ManifestV2 {
        body: Body::Inline(vec![id(1)]),
        ..paged(2, 4)
    };
    assert_eq!(ManifestV2::decode(&inline.encode()), Err(Error::Format));
    let mut b = paged(8, 4).encode();
    b.push(0);
    assert_eq!(ManifestV2::decode(&b), Err(Error::Format));
    let mut b = paged(8, 4).encode();
    b[0] = 3;
    assert_eq!(AnyManifest::decode(&b), Err(Error::Format));
    // A version-2 manifest is not a version-1 one.
    assert!(Manifest::decode(&paged(8, 4).encode()).is_err());
}

#[test]
fn pages_round_trip_and_refuse_malformed() {
    let ids = vec![id(1), id(2)];
    let p = fs::encode_page(&ids);
    assert_eq!(fs::decode_page(&p).unwrap(), ids);
    assert!(fs::decode_page(&fs::encode_page(&[])).is_err());
    assert!(fs::decode_page(&p[..p.len() - 1]).is_err());
    let mut long = p.clone();
    long.push(0);
    assert!(fs::decode_page(&long).is_err());
    assert_eq!(fs::page_capacity(90_000), 5_624);
    // The digest is domain-separated from a plain hash.
    assert_ne!(fs::page_digest(&p), *blake3::hash(&p).as_bytes());
    assert_ne!(
        fs::page_digest(&p),
        fs::page_digest(&fs::encode_page(&[id(1)]))
    );
}

#[test]
fn file_chunks_open_only_with_their_key_epoch_and_id() {
    let k = keys();
    let fk = fs::new_file_key(&mut OsRng).unwrap();
    let sealed = fs::seal_file_chunk(k.fs_id, &fk, 3, &id(1), b"data", &mut OsRng).unwrap();
    assert_eq!(
        fs::open_file_chunk(k.fs_id, &fk, 3, &id(1), &sealed).unwrap(),
        b"data"
    );
    let other = fs::new_file_key(&mut OsRng).unwrap();
    assert!(fs::open_file_chunk(k.fs_id, &other, 3, &id(1), &sealed).is_err());
    assert!(fs::open_file_chunk(k.fs_id, &fk, 4, &id(1), &sealed).is_err());
    assert!(fs::open_file_chunk(k.fs_id, &fk, 3, &id(2), &sealed).is_err());
    assert!(fs::open_file_chunk(k.fs_id + 1, &fk, 3, &id(1), &sealed).is_err());
    // A changed header epoch fails authentication.
    let mut bad = sealed.clone();
    bad[7] ^= 1;
    assert!(fs::open_file_chunk(k.fs_id, &fk, 3 ^ 1, &id(1), &bad).is_err());
    // Not under the fs keys, and fs-key chunks don't open under a file key.
    assert!(fs::open_chunk(&k, &id(1), &sealed).is_err());
    let fs_chunk = fs::seal_chunk(&k, &id(1), b"data", &mut OsRng).unwrap();
    assert!(fs::open_file_chunk(k.fs_id, &fk, k.epoch, &id(1), &fs_chunk).is_err());
}

#[test]
fn manifests_of_both_versions_seal_and_open() {
    let k = keys();
    let m = AnyManifest::V2(paged(10, 4));
    let sealed = fs::seal_any_manifest(&k, &id(1), &id(2), &m, &mut OsRng).unwrap();
    assert_eq!(
        fs::open_any_manifest(&k, &id(1), &id(2), &sealed).unwrap(),
        m
    );
    assert!(fs::open_any_manifest(&k, &id(1), &id(3), &sealed).is_err());
    let v1 = Manifest {
        size: 1,
        chunk_size: 10,
        chunks: vec![id(5)],
    };
    let sealed = fs::seal_manifest(&k, &id(1), &id(2), &v1, &mut OsRng).unwrap();
    assert_eq!(
        fs::open_any_manifest(&k, &id(1), &id(2), &sealed).unwrap(),
        AnyManifest::V1(v1)
    );
}

#[test]
fn paged_write_bytes_differ_from_inline() {
    let a = fs::write_bytes(7, &id(1), &id(2), &[], &[id(3)], b"m");
    let b = fs::write_paged_bytes(7, &id(1), &id(2), &[], &[id(3)], b"m");
    assert_eq!(a[0], 3);
    assert_eq!(b[0], 4);
    assert_eq!(a[1..], b[1..]);
}
