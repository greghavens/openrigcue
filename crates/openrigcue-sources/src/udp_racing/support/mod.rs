//! Building blocks shared by the racing UDP listeners of this module: transport, packet field access,
//! numeric conversions and small algorithms (Salsa20, FlatBuffers, quaternions).

pub mod channels;
pub mod clock;
pub mod dict_channels;
pub mod flatbuffers;
mod idna2003;
pub mod net;
pub mod py_literal;
pub mod pymath;
pub mod quaternion;
pub mod receiver;
pub mod resolver;
pub mod salsa20;
pub mod wire;
