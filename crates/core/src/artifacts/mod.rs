//! Artifact identities and resolution projections independent of storage I/O.

pub mod descriptor;

pub use descriptor::{NativeArtifactDescriptor, ProvisioningMode, descriptors, descriptors_for};
