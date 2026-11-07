//! The case-merge planner every store shares (spec §5.9 "The merge"): given two or
//! more spellings of one name (`Kingdom/` beside `kingdom/`, `Vol 1.cbz` beside
//! `vol 1.mokuro`), which loser files move into the winner, which go, and what blocks.
//!
//! Pure and store-agnostic. A store finds its variant sets, picks the winner, and
//! answers the planner through a [`Host`] (its listings, its identity check, its
//! paths); it journals and runs the [`Plan`] with its own executor. Every store so
//! plans by the same rules, which is the owner rule's "identical behaviour
//! everywhere":
//!
//! - [`names`]: the volume-group split of a name, and the folds names pair under
//!   ([`pair_key`]) and are held in the taken sets under ([`taken_key`]). Every fold
//!   the planner makes is one of these two.
//! - [`plan`]: [`Plan`], its [`Planned`] steps and [`Block`]s, the
//!   [`Plan::fingerprint`] a re-plan is compared by, and the stored [`Extra`].
//! - [`rules`]: [`plan_set`] over a [`SetSpec`], [`plan_dirs`], [`pair`],
//!   [`free_layer`], [`order_stem_members`].
//! - [`policy`]: what the planner asks the server ([`CaseMergePolicy`]).
//! - [`scope`]: what one merge covers ([`Scope`]), the re-plan [`Memo`] and the
//!   recreate-loop guard's limits.

use std::path::PathBuf;

use crate::relpath::RelPath;

pub mod names;
pub mod plan;
pub mod policy;
pub mod rules;
pub mod scope;

#[cfg(test)]
mod tests;

pub use names::{Named, Part, archive_stems, file_key, named, pair_key, split_name, taken_key};
pub use plan::{Block, Defer, Extra, Node, Plan, Planned, StepKind, defer};
pub use policy::{
    CaseMergePolicy, MergeChoice, MergeRecord, ResolveChoice, is_unstamped_layer_id, strip_counter,
};
pub use rules::{
    SetSpec, free_layer, order_stem_members, pair, plan_dirs, plan_folder_set, plan_set,
    plan_stem_set,
};
pub use scope::{
    EVENT_RETAIN, Memo, RECREATE_LIMIT, RECREATE_WINDOW, RECREATING, Scope, folded, last_name,
    parent_of,
};

/// What the planner needs from a store.
pub trait Host {
    /// The store's own facts about a node (the remote's provider node; locally, the
    /// absolute path and its identity).
    type X: Clone;

    /// The live children of the node `id`, by a direct listing (never a cache alone).
    fn list(&self, id: &str) -> Result<Vec<Node<Self::X>>, Defer>;

    /// `a` and `b` hold the same bytes (§5.9 Identical). Folders never are.
    fn identical(&self, a: &Node<Self::X>, b: &Node<Self::X>) -> Result<bool, Defer>;

    /// The store path of `n`, listed at `rel` (what the policy is asked about).
    fn path_of(&self, n: &Node<Self::X>, rel: &RelPath) -> PathBuf;

    /// The bytes a replace of the winner's main sidecar with `n` uploads; `None`
    /// where the store replaces without them (a local rename).
    fn replace_bytes(&self, n: &Node<Self::X>) -> Result<Option<Vec<u8>>, Defer>;
}
