//! What the data path's primitives cost on this host: a table, not an assertion.
//!
//! Every "is this copy worth removing" question in the carrier's budget comes
//! back to four numbers — a copy, an allocation, a `Bytes` handoff and the
//! record layer's AEAD — and they move with the host, the allocator and the
//! record size. This target prices them, in the same shape the engine uses them
//! (1400-byte datagrams, 32 KiB records), so the answer is measured here rather
//! than quoted from a blog.
//!
//! It asserts nothing: the numbers are context, and the model's
//! `cpu_s_per_gbit` is what a claim about the product is made with
//! (AGENTS.md §10). Run it with `just primitives`, which builds release:
//! an unoptimised copy says nothing about the optimised one.
//!
//! The AEAD is `ring`'s, because that is what `snow` uses and therefore what
//! the profile shows (`chacha20_poly1305_seal_avx2`). A stand-in crate with the
//! same name would price a different implementation.

#![expect(
    clippy::expect_used,
    reason = "the table builds its own buffers and keys; a failure to build one \
              is a bug in the table, not an input it has to handle"
)]
#![expect(
    clippy::cast_precision_loss,
    reason = "iteration counts and nanosecond totals are far below 2^52, where \
              an integer stops being exact in an f64"
)]
#![expect(
    clippy::too_many_lines,
    reason = "one cost table reads as one function; splitting it would hide the \
              order the numbers are meant to be read in"
)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};

/// The carrier's MTU-sized payload.
const DATAGRAM: usize = 1400;

/// The record size the carrier's noise layer writes in practice.
const RECORD: usize = 32 * 1024;

/// The record layer seals every record under a fresh nonce; this table uses one
/// fixed value, because it prices the cipher and not the counter.
const NONCE: [u8; 12] = [0u8; 12];

/// How long each case runs. Long enough that a scheduling hiccup is a fraction
/// of a percent; short enough that the whole table is a coffee break.
const BUDGET: Duration = Duration::from_millis(250);

/// Run `body` for `BUDGET` and answer nanoseconds per call.
fn bench<F: FnMut()>(name: &str, mut body: F) -> f64 {
    for _ in 0..1_000 {
        body();
    }
    let mut calls = 0u64;
    let start = Instant::now();
    while start.elapsed() < BUDGET {
        for _ in 0..100 {
            body();
        }
        calls += 100;
    }
    let ns = start.elapsed().as_secs_f64() * 1e9 / calls as f64;
    println!("{name:<44} {ns:>10.1} ns/call");
    ns
}

/// The same, reported per KiB as well, so a copy can be compared with a copy.
fn bench_per_kib<F: FnMut()>(name: &str, bytes: usize, body: F) -> f64 {
    let ns = bench(name, body);
    println!("{:<44} {:>10.2} ns/KiB", "", ns / (bytes as f64 / 1024.0));
    ns
}

#[test]
#[ignore = "a cost table, not an assertion; run it with `just primitives`"]
fn primitives() {
    let source = [0x5au8; DATAGRAM];
    let mut hot = [0u8; DATAGRAM];

    println!("== copies ==");
    bench_per_kib("copy 1400, hot (both sides in L1)", DATAGRAM, || {
        hot.copy_from_slice(&source);
        black_box(&hot);
    });

    // The destination rotates through 64 MiB, so every copy lands on lines the
    // previous pass evicted — what a send buffer larger than the cache does.
    let mut ring = vec![0u8; 64 << 20];
    let mut at = 0usize;
    bench_per_kib(
        "copy 1400, destination cold (64 MiB ring)",
        DATAGRAM,
        || {
            if at + DATAGRAM > ring.len() {
                at = 0;
            }
            ring[at..at + DATAGRAM].copy_from_slice(&source);
            at += DATAGRAM;
            black_box(&ring[at - DATAGRAM]);
        },
    );

    println!("== allocation shapes ==");
    bench_per_kib("Vec::new + extend 1400", DATAGRAM, || {
        let mut piece = Vec::new();
        piece.extend_from_slice(&source);
        black_box(&piece);
    });
    bench_per_kib("vec![0; 1400]", DATAGRAM, || {
        let piece = vec![0u8; DATAGRAM];
        black_box(&piece);
    });

    println!("== the bytes crate ==");
    let shared = bytes::Bytes::from(vec![0x11u8; RECORD]);
    let _ = bytes::Bytes::clone(&shared);
    bench("Bytes::clone (refcount)", || {
        let handle = shared.clone();
        black_box(&handle);
    });
    bench("Bytes::slice (refcount + range)", || {
        let handle = shared.slice(0..DATAGRAM);
        black_box(&handle);
    });
    // Splitting a pooled region is the receive side's handoff. The refill that
    // follows an exhausted pool is the kernel's next `recvmsg`, which the
    // product pays either way, so it is outside the timed region.
    {
        let mut total = Duration::ZERO;
        let mut calls = 0u64;
        for _ in 0..4 {
            let mut pool = bytes::BytesMut::zeroed(8 << 20);
            let start = Instant::now();
            while pool.len() >= DATAGRAM {
                let handle = pool.split_to(DATAGRAM).freeze();
                black_box(&handle);
                calls += 1;
            }
            total += start.elapsed();
        }
        let ns = total.as_secs_f64() * 1e9 / calls as f64;
        println!(
            "{:<44} {ns:>10.1} ns/call",
            "BytesMut::split_to(1400).freeze()"
        );
    }

    println!("== the record layer's AEAD (ring, as snow uses it) ==");
    let key =
        LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &[7u8; 32]).expect("a 32-byte key"));
    // `seal_in_place_append_tag` appends the tag, so the buffer is a `Vec` that
    // is truncated back to its plaintext length between calls.
    let mut record = vec![0u8; DATAGRAM];
    bench_per_kib("seal 1400 in place", DATAGRAM, || {
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(NONCE),
            Aad::empty(),
            &mut record,
        )
        .expect("a buffer with room for the tag");
        black_box(&record);
        record.truncate(DATAGRAM);
    });
    let mut sealed = record.clone();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(NONCE),
        Aad::empty(),
        &mut sealed,
    )
    .expect("a buffer with room for the tag");
    let mut opened = vec![0u8; sealed.len()];
    bench_per_kib("open 1400 in place", DATAGRAM, || {
        opened.copy_from_slice(&sealed);
        key.open_in_place(
            Nonce::assume_unique_for_key(NONCE),
            Aad::empty(),
            &mut opened,
        )
        .expect("a tag this key wrote");
        black_box(&opened);
    });
    let mut big = vec![0u8; RECORD];
    bench_per_kib("seal 32 KiB in place", RECORD, || {
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(NONCE), Aad::empty(), &mut big)
            .expect("a buffer with room for the tag");
        black_box(&big);
        big.truncate(RECORD);
    });

    println!("== containers and clocks ==");
    let mut tree: std::collections::BTreeMap<u64, bytes::Bytes> = std::collections::BTreeMap::new();
    let leaf = bytes::Bytes::from(vec![0x22u8; DATAGRAM]);
    let mut number = 0u64;
    bench("BTreeMap<u64, Bytes> insert + remove", || {
        number = number.wrapping_add(1);
        tree.insert(number, leaf.clone());
        tree.remove(&number);
        black_box(&tree);
    });
    bench("Instant::now() pair", || {
        let a = Instant::now();
        let b = Instant::now();
        black_box(b.saturating_duration_since(a));
    });
}
