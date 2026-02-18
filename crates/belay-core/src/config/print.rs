//! The effective configuration as a TOML table, for `belay config print`.
//!
//! Every setting is written, defaults included, in the units the reader
//! accepts, so parsing the output gives back an equal [`Config`]. Token
//! secrets never appear: tokens are written as their `token_sha256`.

use std::time::Duration;

use belay_common::format_duration;
use belay_conf::{Table, Value};
use belay_json::ToJson;

use super::read::from_json;
use super::Config;
use crate::sha256::to_hex;

pub(super) fn to_table(c: &Config) -> Table {
    let mut root = Table::new();

    let s = &c.server;
    let mut server = Table::new();
    server.insert("listen", s.listen.as_str());
    server.insert("threads", int(s.threads as u64));
    server.insert("max_connections", int(s.max_connections as u64));
    server.insert("max_body", int(s.max_body));
    server.insert("keep_alive", dur(s.keep_alive));
    server.insert("request_timeout", dur(s.request_timeout));
    server.insert("shutdown_grace", dur(s.shutdown_grace));
    root.insert("server", server);

    let st = &c.storage;
    let mut storage = Table::new();
    storage.insert("dir", st.dir.to_string_lossy().into_owned());
    storage.insert("in_memory", st.in_memory);
    storage.insert("sync", st.sync.to_string());
    storage.insert("checkpoint_bytes", int(st.checkpoint_bytes));
    storage.insert("checkpoint_interval", dur(st.checkpoint_interval));
    storage.insert("segment_size", int(st.segment_size));
    storage.insert("recovery", st.recovery.to_string());
    root.insert("storage", storage);

    let mut auth = Table::new();
    auth.insert("disabled", c.auth.disabled);
    let tokens: Vec<Value> = c
        .auth
        .tokens
        .iter()
        .map(|t| {
            let mut token = Table::new();
            token.insert("name", t.name.as_str());
            token.insert("token_sha256", to_hex(&t.digest));
            let scopes: Vec<&str> = t.scopes.iter().map(|s| s.as_str()).collect();
            token.insert("scopes", scopes);
            Value::Table(token)
        })
        .collect();
    if !tokens.is_empty() {
        auth.insert("tokens", Value::Array(tokens));
    }
    root.insert("auth", auth);

    let mut log = Table::new();
    log.insert("level", c.log.filter.to_string());
    log.insert("format", c.log.format.to_string());
    root.insert("log", log);

    let e = &c.engine;
    let mut engine = Table::new();
    engine.insert("tick", dur(e.tick));
    engine.insert("queue_order", e.queue_order.to_string());
    engine.insert("worker_timeout", dur(e.worker_timeout));
    engine.insert("worker_retention", dur(e.worker_retention));
    engine.insert("batch_size", int(e.batch_size as u64));
    engine.insert("tenant_quantum", e.tenant_quantum);
    let mut weights = Table::new();
    for (tenant, weight) in &e.tenant_weights {
        weights.insert(tenant.as_str(), *weight);
    }
    engine.insert("tenant_weights", weights);
    root.insert("engine", engine);

    let mut queues = Table::new();
    for (name, config) in &c.queues {
        if let Some(value) = from_json(&config.to_json()) {
            queues.insert(name.as_str(), value);
        }
    }
    root.insert("queues", queues);

    let sc = &c.scheduler;
    let mut scheduler = Table::new();
    scheduler.insert("enabled", sc.enabled);
    scheduler.insert("tick", dur(sc.tick));
    scheduler.insert("misfire_threshold", dur(sc.misfire_threshold));
    scheduler.insert("dedupe_window", dur(sc.dedupe_window));
    scheduler.insert("retry_delay", dur(sc.retry_delay));
    root.insert("scheduler", scheduler);

    let calendars: Vec<Value> = c
        .calendars
        .iter()
        .filter_map(|cal| from_json(&cal.to_json()))
        .collect();
    if !calendars.is_empty() {
        root.insert("calendars", Value::Array(calendars));
    }
    let schedules: Vec<Value> = c
        .schedules
        .iter()
        .filter_map(|s| from_json(&s.to_json()))
        .collect();
    if !schedules.is_empty() {
        root.insert("schedules", Value::Array(schedules));
    }

    let w = &c.workflows;
    let mut workflows = Table::new();
    workflows.insert("keep_for", dur(w.keep_for));
    workflows.insert("sweep_every", dur(w.sweep_every));
    workflows.insert("dedupe_window", dur(w.dedupe_window));
    root.insert("workflows", workflows);

    let mut events = Table::new();
    events.insert("buffer", int(c.events.buffer as u64));
    events.insert("subscriber_queue", int(c.events.subscriber_queue as u64));
    root.insert("events", events);

    let cl = &c.cluster;
    let mut cluster = Table::new();
    cluster.insert("enabled", cl.enabled);
    cluster.insert("node_id", cl.node_id.as_str());
    cluster.insert("name", cl.name.as_str());
    cluster.insert("gossip_listen", cl.gossip_listen.as_str());
    if let Some(advertise) = cl.advertise {
        cluster.insert("advertise", advertise.to_string());
    }
    if let Some(api_addr) = &cl.api_addr {
        cluster.insert("api_addr", api_addr.as_str());
    }
    cluster.insert("seeds", cl.seeds.clone());
    cluster.insert("profile", cl.profile.to_string());
    cluster.insert("join_grace", dur(cl.join_grace));
    let mut meta = Table::new();
    for (key, value) in &cl.meta {
        meta.insert(key.as_str(), value.as_str());
    }
    cluster.insert("meta", meta);
    root.insert("cluster", cluster);

    let mut metrics = Table::new();
    metrics.insert("enabled", c.metrics.enabled);
    metrics.insert("process", c.metrics.process);
    metrics.insert("refresh", dur(c.metrics.refresh));
    root.insert("metrics", metrics);

    root
}

fn dur(d: Duration) -> Value {
    Value::String(format_duration(d))
}
