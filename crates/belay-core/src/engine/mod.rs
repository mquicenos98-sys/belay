//! [`Engine`]: storage, queue, scheduler, workflows, events, metrics and
//! clustering, started together and driven by background threads.
//!
//! The engine is split by concern: opening everything and applying the
//! configuration (`builder`), the periodic duties and their threads
//! (`duties`), the request-facing methods (`api`) and the worker protocol
//! (`source`). How the parts are wired is described on [`Engine`].

mod api;

mod builder;

mod duties;
