//! The route table: every endpoint of the v1 API with the access it
//! requires, and the router built from it.
//!
//! The table is data so that the scope of every route is visible in one
//! place and checked by tests, rather than scattered over handlers. The
//! modules below hold the handlers, one per resource.

mod admin;

mod calendars;

mod cluster;

mod events;

mod jobs;
