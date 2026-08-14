//! Compile-fail coverage for directionality at the public client boundary.

#[test]
fn stream_direction_is_descriptor_driven() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/trybuild/*.rs");
}
