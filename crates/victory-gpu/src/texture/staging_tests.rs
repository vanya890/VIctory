use super::*;

#[test]
fn staging_uses_smaller_chunks_after_a_large_allocation_is_refused() {
    const AVAILABLE: u64 = 2 << 20;
    const SIZE: u64 = (448 << 10) + 17;
    let mut chunks = Chunks::new();
    let mut attempts = Vec::new();
    for i in 0..64 {
        let (buffer, offset, address) = chunks
            .take(SIZE, |bytes| {
                attempts.push(bytes);
                (bytes <= AVAILABLE).then_some((attempts.len(), 0x1000))
            })
            .expect("the upload fits in the smaller allocation");
        assert_eq!(attempts[buffer - 1], AVAILABLE);
        assert_eq!(offset, (i % 4) * SIZE.next_multiple_of(512));
        assert!(offset + SIZE <= AVAILABLE);
        assert_eq!(address, 0x1000 + offset as usize);
    }
    assert_eq!(&attempts[..4], &[CHUNK, CHUNK / 2, CHUNK / 4, CHUNK / 8]);
    assert_eq!(&attempts[4..], &[AVAILABLE; 16]);

    chunks.recycle();
    let (_, offset, _) = chunks
        .take(SIZE, |_| panic!("the first smaller chunk should be reused"))
        .unwrap();
    assert_eq!(offset, 0);
}

#[test]
fn staging_tries_the_request_size_after_smaller_growth_and_recovers() {
    let mut chunks = Chunks::<u64>::new();
    let mut attempts = Vec::new();
    assert!(chunks.take(CHUNK, |_| None).is_none());
    let size = (9u64 << 20) + 17;
    let rounded = size.next_multiple_of(MIN_CHUNK);
    let (buffer, offset, _) = chunks
        .take(size, |bytes| {
            attempts.push(bytes);
            (bytes == rounded).then_some((bytes, 0))
        })
        .unwrap();
    assert_eq!(attempts, [CHUNK / 2, rounded]);
    assert_eq!((buffer, offset), (rounded, 0));

    chunks.recycle();
    let (buffer, offset, _) = chunks.take(CHUNK, |bytes| Some((bytes, 0))).unwrap();
    assert_eq!((buffer, offset), (CHUNK, 0));
}

#[test]
fn staging_keeps_full_sized_chunks_after_allocations_recover() {
    let mut chunks = Chunks::new();
    for _ in 0..CHUNKS_KEPT {
        let (buffer, ..) = chunks.take(MIN_CHUNK, |bytes| (bytes == MIN_CHUNK).then_some((bytes, 0))).unwrap();
        assert_eq!(buffer, MIN_CHUNK);
    }

    chunks.recycle();
    for _ in 0..CHUNKS_KEPT {
        let (buffer, ..) = chunks.take(CHUNK, |bytes| Some((bytes, 0))).unwrap();
        assert_eq!(buffer, CHUNK);
    }

    chunks.recycle();
    assert_eq!(chunks.all.len(), CHUNKS_KEPT);
    for _ in 0..CHUNKS_KEPT {
        let (buffer, offset, _) = chunks.take(CHUNK, |_| panic!("a full-size chunk should be reused")).unwrap();
        assert_eq!((buffer, offset), (CHUNK, 0));
    }
}

#[test]
fn staging_suballocations_in_a_smaller_d3d12_buffer_do_not_overlap() {
    let Some(host) = host() else { return };
    let mut chunks = Chunks::new();
    let allocate = |bytes| {
        if bytes > MIN_CHUNK {
            return None;
        }
        host.upload_buffer(bytes)
            .map(|(buffer, data)| (buffer, data as usize))
    };
    let (buffer, first, head) = chunks.take(17, allocate).unwrap();
    let (next, second, tail) = chunks.take(17, allocate).unwrap();
    assert_eq!(buffer.as_raw(), next.as_raw());
    assert_eq!((first, second), (0, 512));
    // SAFETY: both offsets describe checked, disjoint staging slices.
    unsafe {
        assert_eq!(buffer.GetDesc().Width, MIN_CHUNK);
        (head as *mut u8).write_bytes(0x19, 17);
        (tail as *mut u8).write_bytes(0x82, 17);
        assert_eq!(
            std::slice::from_raw_parts(head as *const u8, 17),
            &[0x19; 17]
        );
        assert_eq!(
            std::slice::from_raw_parts(tail as *const u8, 17),
            &[0x82; 17]
        );
    }
}
