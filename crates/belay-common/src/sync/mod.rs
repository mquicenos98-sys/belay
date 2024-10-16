//! Thread coordination: poison-tolerant locking, shutdown signalling, a
//! wait group, a fixed-size thread pool and a drift-free ticker.
//!
//! Lock poisoning is treated as recoverable throughout belay: a panic while
//! holding a lock is already reported by the panic hook, and taking the guard
//! anyway beats cascading that panic into every thread that touches the lock.
//! The flip side is that code must not rely on poisoning to hide data it may
//! have left half-updated: mutate under a lock only in steps that cannot panic
//! midway, or restore consistency before anything that can.
//! Use [`lock`], [`read`], [`write()`], [`wait`] and [`wait_timeout`] instead of
//! calling `.unwrap()` on the standard results.

mod pool;

mod shutdown;

mod ticker;
