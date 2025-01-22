//! Graph views of a [`WorkflowSpec`]: ordering, reachability, cycle
//! detection, and the validation built on them.
//!
//! Steps are identified by name; if names repeat (which validation rejects)
//! a dependency resolves to the first step with that name. Dependencies on
//! unknown steps are ignored by the traversals. Every traversal breaks ties
//! by declaration order, so results are deterministic.

use std::collections::{BTreeSet, HashMap, VecDeque};

use belay_json::{FromJsonError, Value};

use super::WorkflowSpec;
use crate::error::quote;
use crate::ids::{LABEL_RULE, STEP_RULE};
use crate::job::validate_meta;
use crate::limits::{MAX_STEP_NAME_LEN, MAX_WORKFLOW_STEPS};
use crate::validate::{Validate, Validator};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Unvisited,
    OnPath,
    Done,
}
