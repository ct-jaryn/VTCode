use mac_notification_sys::*;

#[test]
fn set_application_again_is_idempotent() {
    // Patched contract: a successful registration is reusable. Upstream
    // returned `AlreadySet` on the second call, which callers could not
    // distinguish from a real failure.
    set_application("com.apple.Terminal").unwrap();
    set_application("com.apple.Terminal").unwrap();
}

#[test]
fn get_default_identifier() {
    let bundle = get_bundle_identifier_or_default("thisappdoesnotexist");
    assert_eq!(bundle, "com.apple.Finder");
}
