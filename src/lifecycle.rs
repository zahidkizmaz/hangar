//! What `up`, `down` and `destroy` act on.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resource {
    Bay,
    Proxy,
}
