mod parse;

use peppy_codec::contract::PeppyContract;
use peppy_mcp_catalog::DeclaredMembers;

// The `mcp_exposure/v1` document model lives in the shared `peppy-mcp-catalog`
// crate beside the validation that derives a catalog from it, so the daemon,
// the built-in server and the hub check all read one definition. Re-exported
// here with the daemon's own parser, which maps a file to the daemon's
// error vocabulary the way the contract and pairing parsers do.
pub use parse::PeppyMcpExposureParser;
pub use peppy_mcp_catalog::{
    ActionExposure, ActionOperation, DaemonInterfaceRef, ExposureManifest, ExposureTarget,
    FreshnessPolicy, GoalBound, ImageCodec, ImageFieldMap, ImageRepresentation, JpegQuality, MaxHz,
    McpExposure, OversizePolicy, PinnedContractRef, PublicName, RestrictBounds, ServerIdentity,
    ServiceExposure, ServiceOperation, TargetSource, TopicExposure, UpdatePolicy,
};

/// The members of `contract` as the MCP catalog validates an exposure
/// against them: their shapes, failure kinds and marks.
pub fn declared_members(contract: &PeppyContract) -> DeclaredMembers<'_> {
    DeclaredMembers {
        topics: &contract.interfaces.topics,
        services: &contract.interfaces.services,
        actions: &contract.interfaces.actions,
        failure_kinds: &contract.failure_kinds,
        marks: &contract.marks,
    }
}
