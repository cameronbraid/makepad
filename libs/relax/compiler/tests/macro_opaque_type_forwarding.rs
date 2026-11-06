// A type captured by macro_rules is opaque to a downstream literal matcher.
// Facet's vtable builders rely on falling through to the `$ty:ty` unwrap arm.
macro_rules! unwrap_field {
    ($value:expr, Option<$inner:ty>) => { $value };
    ($value:expr, $ty:ty) => {
        match $value { Some(value) => value, None => panic!("missing field") }
    };
}

macro_rules! build_field {
    ($value:expr, $ty:ty) => { unwrap_field!($value, $ty) };
}

fn plus_one(n: u8) -> u8 { n + 1 }

#[test]
fn forwarded_optional_type_uses_the_type_fragment_arm() {
    let value: Option<fn(u8) -> u8> = build_field!(Some(Some(plus_one as fn(u8) -> u8)), Option<fn(u8) -> u8>);
    assert_eq!(value.unwrap()(4), 5);
    let value: u8 = build_field!(Some(7u8), u8);
    assert_eq!(value, 7);
}

macro_rules! classify_type {
    (Option<$inner:ty>) => { 1 };
    (u8) => { 2 };
    ($other:ty) => { 3 };
}
macro_rules! forward_type {
    ($ty:ty) => { classify_type!($ty) };
}
macro_rules! relay_type {
    ($ty:tt) => { classify_type!($ty) };
}
macro_rules! forward_twice {
    ($ty:ty) => { relay_type!($ty) };
}

#[test]
fn forwarded_types_remain_opaque_through_token_tree_recapture() {
    assert_eq!(classify_type!(Option<u8>), 1);
    assert_eq!(classify_type!(u8), 2);
    assert_eq!(forward_type!(Option<u8>), 3);
    assert_eq!(forward_type!(u8), 3);
    assert_eq!(forward_twice!(Option<u8>), 3);
    assert_eq!(forward_twice!(u8), 3);
}
