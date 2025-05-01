//! [`QueueEngine`]: the shared handle every part of belay talks to.
//!
//! The engine is split by concern into submodules that all add methods to
//! the same type: opening and index rebuild (`open`), enqueueing
//! (`enqueue`), leasing with long polls (`lease`), the worker protocol
//! (`work`), administrative changes (`admin`), reads (`read`) and the
//! periodic `tick`.
//!
//! Every mutating method follows the same shape, implemented once in
//! [`QueueEngine::transact`]:
//!
//! 1. take the engine lock and read the clock;
//! 2. stage the change in a [`Ctx`], reading jobs from the store as needed;
//! 3. write the staged [`Txn`](crate::txn::Txn) as one batch;
//! 4. apply it to the in-memory state and emit its events;
//! 5. release the lock, then wake long polls and run `on_finished` hooks.
//!
//! A failure in 2 or 3 returns before 4, leaving memory untouched.

mod admin;

mod enqueue;

mod lease;

mod open;

mod read;

mod tick;
