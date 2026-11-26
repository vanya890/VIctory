use super::*;
use crate::watch::PAGE;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};

/// Two adjacent live allocations, ordinary committed memory followed by a
/// write-watched pool.
fn mixed_pages() -> usize {
    let flags = MEM_RESERVE | MEM_COMMIT;
    for _ in 0..16 {
        // SAFETY: ask Windows for a candidate address. Another allocator
        // may win it after release, so check both following reservations.
        unsafe {
            let slot = VirtualAlloc(std::ptr::null(), 3 * PAGE, MEM_RESERVE, PAGE_READWRITE);
            assert!(!slot.is_null());
            assert_ne!(VirtualFree(slot, 0, MEM_RELEASE), 0);
            let ordinary = VirtualAlloc(slot, PAGE, flags, PAGE_READWRITE);
            if ordinary.is_null() {
                continue;
            }
            let pool = (ordinary as usize + PAGE) as *mut std::ffi::c_void;
            if !VirtualAlloc(pool, 2 * PAGE, flags | crate::watch::tests::MEM_WRITE_WATCH, PAGE_READWRITE).is_null() {
                return ordinary as usize;
            }
            assert_ne!(VirtualFree(ordinary, 0, MEM_RELEASE), 0);
        }
    }
    panic!("could not allocate adjacent watched and ordinary pages");
}

fn check_rows(address: usize, mixed: bool) {
    let host = crate::host::host().unwrap();
    const WIDTH: usize = 64;
    const ROWS_PER_PAGE: usize = PAGE / (WIDTH * 4);
    const HEIGHT: usize = 3 * ROWS_PER_PAGE;
    let fill = |page: usize, value: u8| {
        let at = address + page * PAGE;
        let _writing = crate::watch::writing(at, PAGE);
        // SAFETY: each of the three pages is a live allocation owned by this test.
        unsafe { std::ptr::write_bytes(at as *mut u8, value, PAGE) };
    };
    let object = texture_at_address(address as u64, &texture_words(WIDTH as u32, HEIGHT as u32, 28, 0x100, 0));
    let (_, desc) = crate::resource::texture_memory(object).unwrap();
    let layout = crate::resource::texture_layout(&desc).unwrap();
    let layout = &layout.planes[0].layout;
    assert_eq!(layout.offset(0, 0, ROWS_PER_PAGE as u32, 0).unwrap(), PAGE as u64);
    assert_eq!(layout.size, (3 * PAGE) as u64);
    let copy = crate::resource::host_copy(object).unwrap();
    let staging = crate::texture::staging(object, &copy.texture).unwrap();
    let ordinary = crate::texture::tests::untracked_bytes(&staging);
    if mixed {
        assert_eq!(ordinary, [(address, address + PAGE)]);
    } else {
        assert!(ordinary.is_empty());
    }
    for page in 0..3 {
        fill(page, 0x33);
    }
    let read = compile_cs(READ);
    let slots = Slots::new(1);
    let mut srv = [0; 10];
    srv[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
    let view = View::Shader(object as usize, Some(srv));
    slots.make(0, &view);
    let (output, mapped) = host.shared_buffer((WIDTH * HEIGHT * 16) as u64).unwrap();
    let submit = |expected: [u8; 3], copies| {
        let list = host.new_list().unwrap();
        readable(&list, &used(ResourceType::Srv), &view, false);
        run(&list, &slots, &read, 0, Some(&output), (8, HEIGHT as u32 / 8));
        list.close();
        let before = crate::texture::copied(object);
        host.execute(&[&list]);
        // SAFETY: the completed dispatch wrote WIDTH * HEIGHT float4s.
        let pixels = unsafe { std::slice::from_raw_parts(mapped.cast::<[f32; 4]>(), WIDTH * HEIGHT) };
        for (page, group) in pixels.chunks(WIDTH * ROWS_PER_PAGE).enumerate() {
            let want = f32::from(expected[page]) / 255.0;
            let wrong = group.iter().filter(|p| !p.iter().all(|&v| (v - want).abs() <= 1e-6)).count();
            assert_eq!(wrong, 0, "page {page}, expected {expected:02x?}");
        }
        assert_eq!(crate::texture::copied(object) - before, copies);
    };
    submit([0x33; 3], 1);
    if mixed {
        fill(0, 0x99);
        submit([0x99, 0x33, 0x33], 1);
        submit([0x99, 0x33, 0x33], 0);
    } else {
        fill(0, 0x66);
        crate::texture::tests::defer_one_upload(&staging);
        submit([0x66, 0x33, 0x33], 1);
        fill(0, 0x77);
        crate::texture::tests::defer_one_upload(&staging);
        fill(1, 0x88);
        submit([0x77, 0x88, 0x33], 1);
        submit([0x77, 0x88, 0x33], 0);
    }
    drop(staging);
    drop(copy);
    crate::object::release(object);
}

#[test]
fn mixed_pages_and_deferred_rows_reach_the_sampled_texture() {
    if crate::host::host().is_none() {
        return;
    }
    // Both stay allocated, as in the other draw tests: the tracked pages' guards
    // outlive the textures, and a later test's pool at a freed address would
    // never see its writes.
    let mixed = mixed_pages();
    let tracked = crate::watch::tests::pool(3);
    check_rows(mixed, true);
    check_rows(tracked, false);
}

/// BC1 colors as RGB565 endpoints, and what they decode to.
const RED: u16 = 0xF800;
const GREEN: u16 = 0x07E0;
const BLUE: u16 = 0x001F;
const YELLOW: u16 = 0xFFE0;
const WHITE: u16 = 0xFFFF;

fn decoded(color: u16) -> [f32; 4] {
    let (r, g, b) = (f32::from(color >> 11), f32::from(color >> 5 & 63), f32::from(color & 31));
    [r / 31.0, g / 63.0, b / 31.0, 1.0]
}

/// A block-compressed texture whose block rows span the page boundaries: a
/// write into one page copies only the 4x4 block rows it covers, and the
/// other pages keep what they had.
fn check_block_rows(address: usize, mixed: bool) {
    let host = crate::host::host().unwrap();
    const WIDTH: usize = 64;
    let words = |height: usize| texture_words(WIDTH as u32, height as u32, 71, 0x100, 0);
    // Layout rows of a block-compressed texture are 4x4 block rows, and the
    // pitch (bytes of one) is the layout's, so the page geometry follows it.
    let probe = texture_at_address(address as u64, &words(32));
    let (_, desc) = crate::resource::texture_memory(probe).unwrap();
    let pitch = crate::resource::texture_layout(&desc).unwrap().planes[0].layout.offset(0, 0, 1, 0).unwrap() as usize;
    crate::object::release(probe);
    assert!(pitch >= WIDTH / 4 * 8 && PAGE.is_multiple_of(pitch), "block row pitch {pitch}");
    let rows_per_page = PAGE / pitch * 4;
    let height = 3 * rows_per_page;
    let fill = |page: usize, color: u16| {
        let at = address + page * PAGE;
        let block = [color.to_le_bytes(), color.to_le_bytes(), [0; 2], [0; 2]].concat();
        let _writing = crate::watch::writing(at, PAGE);
        for offset in (0..PAGE).step_by(8) {
            // SAFETY: each of the three pages is a live allocation owned by this test.
            unsafe { std::ptr::copy_nonoverlapping(block.as_ptr(), (at + offset) as *mut u8, 8) };
        }
    };
    let object = texture_at_address(address as u64, &words(height));
    let (_, desc) = crate::resource::texture_memory(object).unwrap();
    let layout = crate::resource::texture_layout(&desc).unwrap();
    let layout = &layout.planes[0].layout;
    assert_eq!(layout.offset(0, 0, rows_per_page as u32 / 4, 0).unwrap(), PAGE as u64);
    assert_eq!(layout.size, (3 * PAGE) as u64);
    let copy = crate::resource::host_copy(object).unwrap();
    let staging = crate::texture::staging(object, &copy.texture).unwrap();
    let ordinary = crate::texture::tests::untracked_bytes(&staging);
    if mixed {
        assert_eq!(ordinary, [(address, address + PAGE)]);
    } else {
        assert!(ordinary.is_empty());
    }
    for page in 0..3 {
        fill(page, [RED, GREEN, BLUE][page]);
    }
    let read = compile_cs(READ);
    let slots = Slots::new(1);
    let mut srv = [0; 10];
    srv[..6].copy_from_slice(&[71, 4, 0x1688, 0, 0, 1]);
    let view = View::Shader(object as usize, Some(srv));
    slots.make(0, &view);
    let (output, mapped) = host.shared_buffer((WIDTH * height * 16) as u64).unwrap();
    let submit = |expected: [u16; 3], copies| {
        let list = host.new_list().unwrap();
        readable(&list, &used(ResourceType::Srv), &view, false);
        run(&list, &slots, &read, 0, Some(&output), (8, height as u32 / 8));
        list.close();
        let before = crate::texture::copied(object);
        host.execute(&[&list]);
        // SAFETY: the completed dispatch wrote WIDTH * height float4s.
        let pixels = unsafe { std::slice::from_raw_parts(mapped.cast::<[f32; 4]>(), WIDTH * height) };
        for (page, group) in pixels.chunks(WIDTH * rows_per_page).enumerate() {
            let want = decoded(expected[page]);
            let wrong = group.iter().filter(|p| !p.iter().zip(want).all(|(&v, w)| (v - w).abs() <= 1e-6)).count();
            assert_eq!(wrong, 0, "page {page}, expected {:04x}", expected[page]);
        }
        assert_eq!(crate::texture::copied(object) - before, copies);
    };
    submit([RED, GREEN, BLUE], 1);
    if mixed {
        fill(0, YELLOW);
        submit([YELLOW, GREEN, BLUE], 1);
        submit([YELLOW, GREEN, BLUE], 0);
    } else {
        fill(1, YELLOW);
        submit([RED, YELLOW, BLUE], 1);
        fill(2, WHITE);
        crate::texture::tests::defer_one_upload(&staging);
        fill(0, WHITE);
        submit([WHITE, YELLOW, WHITE], 1);
        submit([WHITE, YELLOW, WHITE], 0);
    }
    drop(staging);
    drop(copy);
    crate::object::release(object);
}

#[test]
fn block_compressed_rows_keep_their_block_alignment_across_pages() {
    if crate::host::host().is_none() {
        return;
    }
    // The pools stay allocated, as in the test above.
    let mixed = mixed_pages();
    let tracked = crate::watch::tests::pool(3);
    check_block_rows(mixed, true);
    check_block_rows(tracked, false);
}
