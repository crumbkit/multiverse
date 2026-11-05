use std::cell::RefCell;
use std::ffi::c_void;

use super::{Failure, MOST_LISTING_TRIES, listing};

/// Returns a fake libproc listing call over `tries`, one for each try. Each
/// try's sizing call returns its first value, as if the list held that
/// many bytes then, and its reading call writes as much of its list as
/// fits into the buffer and returns what it wrote, as libproc does.
/// `sizes` records each reading call's buffer size.
fn fake<'a>(
    lists: &'a [(i32, Vec<u8>)],
    sizes: &'a RefCell<Vec<usize>>,
) -> impl Fn(*mut c_void, i32) -> i32 + 'a {
    let tries = RefCell::new(0_usize);
    move |buffer, size| {
        let (sized, list) = &lists[(*tries.borrow()).min(lists.len() - 1)];
        if buffer.is_null() {
            return *sized;
        }
        *tries.borrow_mut() += 1;
        let size = usize::try_from(size).unwrap();
        sizes.borrow_mut().push(size);
        let written = list.len().min(size);
        // SAFETY: `listing` passes a live, writable buffer of `size`
        // bytes, and this writes `written` bytes, at most `size`.
        #[expect(
            unsafe_code,
            reason = "the fake writes into the buffer that listing passes, as libproc does"
        )]
        unsafe {
            std::ptr::copy_nonoverlapping(list.as_ptr(), buffer.cast::<u8>(), written);
        }
        i32::try_from(written).unwrap()
    }
}

#[test]
fn a_list_that_fits_its_buffer_is_read_in_one_try() {
    let lists = [(40, vec![7_u8; 40])];
    let sizes = RefCell::new(Vec::new());
    assert_eq!(listing("fake", fake(&lists, &sizes)).unwrap(), lists[0].1);
    assert_eq!(sizes.borrow().len(), 1);
}

#[test]
fn a_list_that_fills_its_buffer_is_read_again_into_a_larger_one() {
    // Both sizing calls see 40 bytes, and the list grows past that size
    // and its slack before the first reading call. The second try's buffer
    // is twice the first, so the grown list fits.
    let grown = vec![2_u8; 40 + 32 * 8 + 100];
    let lists = [(40, grown.clone()), (40, grown.clone())];
    let sizes = RefCell::new(Vec::new());
    let read = listing("fake", fake(&lists, &sizes)).unwrap();
    assert_eq!(read, grown);
    let sizes = sizes.borrow();
    assert_eq!(sizes.len(), 2);
    assert!(sizes[1] >= 2 * sizes[0], "{sizes:?}");
}

#[test]
fn a_list_that_fills_every_buffer_is_an_error_after_the_last_try() {
    let sizes = RefCell::new(Vec::new());
    let always_full = |buffer: *mut c_void, size: i32| {
        if buffer.is_null() {
            8
        } else {
            sizes.borrow_mut().push(usize::try_from(size).unwrap());
            size
        }
    };
    match listing("fake", always_full) {
        Err(Failure::Growing { call, tries }) => {
            assert_eq!(call, "fake");
            assert_eq!(tries, MOST_LISTING_TRIES);
        }
        other => panic!("expected a growing list, got {other:?}"),
    }
    assert_eq!(sizes.borrow().len(), MOST_LISTING_TRIES);
}
