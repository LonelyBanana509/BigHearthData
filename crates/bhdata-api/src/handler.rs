//! The request seam (wire-protocol spec, section 6): what the gateway hands to the rest of the
//! system, and what comes back. No implementation here. At M5 the handler is thin wiring in
//! `bhdata-server`; the planner replaces it in M6 and the wire does not change.

use std::future::Future;

use bhdata_recipe::canon::Json;
use bhdata_types::{Error, LoginId, RecipeVersion, RequestId, SubjectId, Timestamp};

use crate::shard::Outcome;

/// The authenticated login a request came from. The gateway has already checked its permission,
/// recipe allow-list and rate limit (spec, decision 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The login the connection authenticated as.
    pub login_id: LoginId,
}

/// One `request` message, parsed and checked for shape, with the arguments still as exact JSON
/// (integers never went through a float). The handler converts them by the plan's argument types.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The client's request id.
    pub request_id: RequestId,
    /// When the client created the request, corrected by the server clock offset (spec, decision 12).
    pub created_at: Timestamp,
    /// The recipe name.
    pub recipe: String,
    /// The version the caller named, or `None` for the latest.
    pub version: Option<RecipeVersion>,
    /// The shard key of the subject.
    pub subject: SubjectId,
    /// The shard key of the counterparty, if the recipe has one.
    pub counterparty: Option<SubjectId>,
    /// The arguments, a JSON object.
    pub args: Json,
    /// The client's shard-map version. Accepted and ignored at stage 0.
    pub map_version: Option<u64>,
}

/// What the gateway sends back: the stored outcome of the command (done or failed, the exact
/// stored result, and whether it is a replay).
pub type Reply = Outcome;

/// The one thing the gateway knows about the rest of the system.
pub trait RequestHandler: Send + Sync {
    /// Runs one request to its final outcome, or says why it could not (`overloaded`,
    /// `request_expired`, `request_id_conflict`, `bad_arguments`, `unknown_recipe`, `internal`, ...).
    /// An `Err` means nothing was stored, so the caller may retry with the same request id.
    fn handle(
        &self,
        caller: Caller,
        call: Call,
    ) -> impl Future<Output = Result<Reply, Error>> + Send;
}
