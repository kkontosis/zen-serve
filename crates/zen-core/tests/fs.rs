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

// ------------------------------------------------------------------ version 3

use zen_core::fs::{BodyV3, CdcParams, ManifestV3, PageRefV3};

const CDC: CdcParams = CdcParams {
    min: 64,
    avg_bits: 8,
    max: 1000,
};

fn v3_inline(lens: &[u32]) -> ManifestV3 {
    ManifestV3 {
        size: lens.iter().map(|&l| l as u64).sum(),
        n: lens.len() as u64,
        cdc: CDC,
        file_key: None,
        body: BodyV3::Inline(
            lens.iter()
                .enumerate()
                .map(|(i, &l)| (id(i as u8), l))
                .collect(),
        ),
    }
}

fn v3_paged(pages: &[(u32, u32)], page_size: u32) -> ManifestV3 {
    ManifestV3 {
        size: pages.iter().map(|&(b, _)| b as u64).sum(),
        n: pages.iter().map(|&(_, c)| c as u64).sum(),
        cdc: CDC,
        file_key: Some(FileKey {
            key_epoch: 2,
            key: [5; 32],
        }),
        body: BodyV3::Paged {
            page_size,
            pages: pages
                .iter()
                .enumerate()
                .map(|(i, &(bytes, count))| PageRefV3 {
                    id: id(i as u8),
                    digest: [i as u8; 32],
                    bytes,
                    count,
                })
                .collect(),
        },
    }
}

#[test]
fn manifest_v3_round_trips() {
    for m in [
        v3_inline(&[100, 1000, 7]),
        v3_inline(&[]),
        v3_paged(&[(3000, 4), (400, 1)], 4),
        v3_paged(&[], 4),
    ] {
        assert_eq!(ManifestV3::decode(&m.encode()).unwrap(), m);
        let any = AnyManifest::V3(m.clone());
        assert_eq!(AnyManifest::decode(&any.encode()).unwrap(), any);
    }
    // Sealed and opened like the other versions.
    let fsk = keys();
    let m = AnyManifest::V3(v3_inline(&[10, 20]));
    let sealed = fs::seal_any_manifest(&fsk, &id(1), &id(2), &m, &mut OsRng).unwrap();
    assert_eq!(
        fs::open_any_manifest(&fsk, &id(1), &id(2), &sealed).unwrap(),
        m
    );
}

#[test]
fn manifest_v3_refusals() {
    let bad = |m: &ManifestV3| ManifestV3::decode(&m.encode());
    // A length of 0, or above max.
    assert_eq!(bad(&v3_inline(&[10, 0])), Err(Error::Format));
    assert_eq!(bad(&v3_inline(&[1001])), Err(Error::Format));
    // Lengths or page bytes that don't sum to size.
    let mut m = v3_inline(&[10, 20]);
    m.size = 31;
    assert_eq!(bad(&m), Err(Error::Format));
    let mut m = v3_paged(&[(3000, 4), (400, 1)], 4);
    m.size += 1;
    assert_eq!(bad(&m), Err(Error::Format));
    // n = 0 with size > 0, and the reverse.
    let mut m = v3_inline(&[]);
    m.size = 5;
    assert_eq!(bad(&m), Err(Error::Format));
    let mut m = v3_paged(&[(3000, 4), (400, 1)], 4);
    m.n = 0;
    assert_eq!(bad(&m), Err(Error::Format));
    // Page counts: 0, above page_size, not summing to n, or with bytes a
    // count of chunks can't hold; pages need not be full.
    assert_eq!(
        bad(&v3_paged(&[(3000, 4), (400, 0)], 4)),
        Err(Error::Format)
    );
    assert_eq!(
        bad(&v3_paged(&[(3000, 5), (400, 1)], 4)),
        Err(Error::Format)
    );
    assert_eq!(
        bad(&v3_paged(&[(3000, 4), (400, 1)], 0)),
        Err(Error::Format)
    );
    assert_eq!(bad(&v3_paged(&[(3, 4), (400, 1)], 4)), Err(Error::Format));
    assert_eq!(
        bad(&v3_paged(&[(4001, 4), (400, 1)], 4)),
        Err(Error::Format)
    );
    let mut m = v3_paged(&[(3000, 4), (400, 1)], 4);
    m.n = 6;
    assert_eq!(bad(&m), Err(Error::Format));
    for pages in [
        &[(3000, 3), (400, 1)][..],
        &[(10, 1), (20, 2), (30, 4)],
        &[(5, 1)],
    ] {
        let m = v3_paged(pages, 4);
        assert_eq!(ManifestV3::decode(&m.encode()), Ok(m), "{pages:?}");
    }
    // Parameters.
    for cdc in [
        CdcParams { min: 63, ..CDC },
        CdcParams { avg_bits: 7, ..CDC },
        CdcParams {
            avg_bits: 25,
            max: u32::MAX,
            ..CDC
        },
        CdcParams { min: 256, ..CDC },
        CdcParams { max: 256, ..CDC },
    ] {
        let mut m = v3_inline(&[10]);
        m.cdc = cdc;
        assert_eq!(bad(&m), Err(Error::Format), "{cdc:?}");
    }
    // key_mode, form, trailing bytes, the version byte.
    let enc = v3_inline(&[10]).encode();
    let mut b = enc.clone();
    b[26] = 2; // key_mode
    assert_eq!(ManifestV3::decode(&b), Err(Error::Format));
    let mut b = enc.clone();
    b[27] = 3; // form
    assert_eq!(ManifestV3::decode(&b), Err(Error::Format));
    let mut b = enc.clone();
    b.push(0);
    assert_eq!(ManifestV3::decode(&b), Err(Error::Format));
    let mut b = enc;
    b[0] = 2;
    assert!(ManifestV3::decode(&b).is_err());
}

#[test]
fn page_forms() {
    let e = vec![(id(1), 100u32), (id(2), 7)];
    let p2 = fs::encode_page2(&e);
    assert_eq!(p2.len(), 4 + 20 * 2);
    assert_eq!(fs::decode_page2(&p2).unwrap(), e);
    let any = fs::decode_any_page(&p2).unwrap();
    assert_eq!(
        (any.ids, any.lens),
        (vec![id(1), id(2)], Some(vec![100, 7]))
    );
    // Form 1 is told apart, and each decoder refuses the other form.
    let p1 = fs::encode_page(&[id(1), id(2)]);
    assert_eq!(fs::decode_any_page(&p1).unwrap().lens, None);
    assert!(fs::decode_page(&p2).is_err());
    assert!(fs::decode_page2(&p1).is_err());
    // Empty, a zero length, a wrong length.
    assert!(fs::decode_page2(&fs::encode_page2(&[])).is_err());
    assert!(fs::decode_page2(&fs::encode_page2(&[(id(1), 0)])).is_err());
    let mut long = p2.clone();
    long.push(0);
    assert!(fs::decode_page2(&long).is_err());
    assert!(fs::decode_any_page(&[]).is_err());
    assert_eq!(fs::page_capacity2(90_000), 4_499);
    assert_eq!(fs::page_capacity(90_000), 5_624);
}
