//! `GET /v1/cluster/members`: the cluster as this node sees it, one page
//! at a time, ordered by node id. A node that is not clustered lists
//! itself as the only (and leading) member.
//!
//! Views converge through gossip but are not agreed on at any instant, so
//! two nodes may briefly list different states for the same member.

use belay_core::Paging;
use belay_http::{Request, Response};
use belay_proto::api::{ApiError, Cursor, MemberInfo, Page};
use belay_proto::limits::DEFAULT_PAGE_LIMIT;

use crate::app::App;
use crate::extract;
use crate::reply;

pub(super) fn members(app: &App, req: &mut Request) -> Result<Response, ApiError> {
    let paging = extract::paging(req, &[])?;
    Ok(reply::ok(&page_of(app.engine.members(), &paging)?))
}

/// One page of `members` by id, after `paging`'s cursor.
fn page_of(mut members: Vec<MemberInfo>, paging: &Paging) -> Result<Page<MemberInfo>, ApiError> {
    let after = match &paging.cursor {
        None => None,
        Some(cursor) => Some(cursor.as_name().ok_or_else(|| {
            ApiError::invalid_argument("$.cursor: was not issued by this listing")
        })?),
    };
    // Paging::from_pairs has already checked the range.
    let limit = paging.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    members.retain(|m| after.map_or(true, |a| m.id.as_str() > a));
    members.sort_by(|a, b| a.id.cmp(&b.id));
    members.truncate(limit as usize + 1);
    Ok(Page::paginate(members, limit, |m| Cursor::from_name(&m.id)))
}
