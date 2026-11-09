//! Checks that reading the process-global catalog before one is installed does not stop the
//! install, which is how a provider built before `tars.init()` made every later init fail.
//!
//! The catalog is process-global, so this binary holds this one test.

use std::path::Path;

use tars_config::{catalog, install_catalog, load_catalog};

#[test]
fn a_catalog_read_before_install_leaves_the_install_to_succeed() {
    let early: *const _ = catalog();

    let home =
        load_catalog(Path::new("/nonexistent/tars/home")).expect("no library is the shipped layer");
    install_catalog(home).expect("an early read must not occupy the installed catalog");

    let late: *const _ = catalog();
    assert!(
        !std::ptr::eq(early, late),
        "after the install, readers get the installed catalog"
    );
}
