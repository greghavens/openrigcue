//! Re-export of the shared TMBS protocol implementation ([`crate::motion::tmbs`]) under this module's path.

#[cfg(test)]
pub(crate) use crate::motion::tmbs::testing;
pub use crate::motion::tmbs::*;
