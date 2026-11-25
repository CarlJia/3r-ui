//! VLESS share-link and QR-code rendering.
//!
//! Both pieces are pure functions over already-loaded DB rows so they are
//! straightforward to exercise in tests without an HTTP boundary in between.

pub mod hy2;
pub mod mieru;
pub mod qr;
pub mod vless;
