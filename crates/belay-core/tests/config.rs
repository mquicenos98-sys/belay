//! Loading configuration: defaults, the environment overlay, located
//! errors, warnings for unknown keys, and printing it back out.

use std::time::Duration;

use belay_common::log::LogFormat;
use belay_core::config::{ClusterProfile, ConfigError, Scope};
use belay_core::Config;
use belay_queue::QueueOrder;
use belay_store::{RecoveryMode, SyncPolicy};

fn load(src: &str) -> Config {
    Config::from_source("belay.toml", src).unwrap_or_else(|e| panic!("{e}"))
}

fn error(src: &str) -> ConfigError {
    Config::from_source("belay.toml", src).expect_err("configuration should be rejected")
}

#[test]
fn an_empty_file_gives_the_defaults() {
    let config = load("");
    assert_eq!(config, Config::default());
    assert_eq!(config.server.listen, "127.0.0.1:7700");
    assert_eq!(config.storage.sync, SyncPolicy::Always);
    assert!(!config.storage.in_memory);
    assert!(!config.auth.is_enforced());
    assert!(!config.cluster.enabled);
    assert!(Config::dev().storage.in_memory);
}

#[test]
fn every_section_is_read() {
    let config = load(
        r#"
        [server]
        listen = "0.0.0.0:8080"
        threads = 4
        max_connections = 64
        max_body = "1MiB"
        keep_alive = "5s"
        request_timeout = 10
        shutdown_grace = "2s"

        [storage]
        dir = "/var/lib/belay"
        sync = "interval:25ms"
        checkpoint_bytes = 0
        checkpoint_interval = "10m"
        segment_size = "8MiB"
        recovery = "skip-corrupt"

        [auth]
        [[auth.tokens]]
        name = "ops"
        token = "ops-token-ops-token"
        scopes = ["admin"]

        [log]
        level = "debug,belay_store=trace"
        format = "json"

        [engine]
        tick = "100ms"
        queue_order = "in_order"
        tenant_weights = { acme = 4 }

        [scheduler]
        enabled = false
        misfire_threshold = "5m"

        [workflows]
        keep_for = "2d"

        [events]
        buffer = 50
        subscriber_queue = 5

        [cluster]
        enabled = true
        node_id = "node-a"
        api_addr = "10.0.0.5:8080"
        seeds = ["10.0.0.6:7946"]
        profile = "wan"

        [metrics]
        process = false
        "#,
    );
    assert_eq!(config.server.threads, 4);
    assert_eq!(config.server.max_body, 1 << 20);
    assert_eq!(config.server.request_timeout, Duration::from_secs(10));
    assert_eq!(
        config.storage.sync,
        SyncPolicy::Interval(Duration::from_millis(25))
    );
    assert_eq!(config.storage.recovery, RecoveryMode::SkipCorrupt);
    assert_eq!(config.storage.store_options().checkpoint_bytes(), None);
    assert!(config.auth.is_enforced());
    assert!(config.auth.tokens[0].scopes.permits(Scope::Work));
    assert_eq!(config.log.format, LogFormat::Json);
    assert_eq!(config.engine.queue_order, QueueOrder::InOrder);
    assert_eq!(config.engine.tenant_weights.len(), 1);
    assert!(!config.scheduler.enabled);
    assert_eq!(config.workflows.keep_for, Duration::from_secs(2 * 86_400));
    assert_eq!(config.events.buffer, 50);
    assert_eq!(config.cluster.profile, ClusterProfile::Wan);
    assert_eq!(
        config.cluster.api_url(&config.server),
        "http://10.0.0.5:8080"
    );
    assert!(!config.metrics.process);
    assert!(config.warnings().is_empty(), "{:?}", config.warnings());
}

#[test]
fn environment_variables_override_the_file() {
    let src = "[server]\nlisten = \"127.0.0.1:7700\"\n\n[queues.emails]\nmax_concurrency = 2\n";
    let vars = [
        ("BELAY_SERVER__THREADS", "3"),
        ("BELAY_STORAGE__IN_MEMORY", "true"),
        ("BELAY_QUEUES__EMAILS__MAX_CONCURRENCY", "9"),
        ("BELAY_LOG__LEVEL", "warn"),
        ("BELAY_URL", "http://ignored:7700"),
        ("OTHER_SERVER__THREADS", "99"),
    ];
    let config = Config::from_source_with_env("belay.toml", src, vars).unwrap();
    assert_eq!(config.server.threads, 3);
    assert!(config.storage.in_memory);
    assert_eq!(config.queues["emails"].max_concurrency, Some(9));
    assert_eq!(config.log.filter.to_string(), "warn");
}

#[test]
fn a_bad_environment_value_names_the_variable() {
    let err = Config::from_source_with_env("belay.toml", "", [("BELAY_SERVER__THREADS", "many")])
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("expected integer at server.threads, found string"),
        "{text}"
    );
    assert!(
        text.contains("environment variable BELAY_SERVER__THREADS"),
        "{text}"
    );
}

#[test]
fn errors_point_at_the_offending_line() {
    let cases = [
        (
            "[server]\nlisten = \"localhost\"\n",
            "invalid value for server.listen: expected host:port, got \"localhost\"",
            (2, 10),
        ),
        (
            "[server]\nthreads = 0\n",
            "invalid value for server.threads: must be at least 1",
            (2, 11),
        ),
        (
            "[storage]\nsync = \"sometimes\"\n",
            "invalid value for storage.sync: invalid sync policy \"sometimes\" (expected always, never or interval:<duration> such as interval:10ms)",
            (2, 8),
        ),
        (
            "[engine]\ntick = \"soon\"\n",
            "invalid value for engine.tick: invalid duration \"soon\": expected a number at byte 0",
            (2, 8),
        ),
        (
            "[cluster]\nprofile = \"moon\"\n",
            "invalid value for cluster.profile: unknown cluster profile \"moon\", expected lan, wan or local",
            (2, 11),
        ),
        (
            "[queues.emails]\nmax_concurency = 3\n",
            "invalid value for queues.emails.max_concurency: unknown field \"max_concurency\" in QueueConfig, did you mean \"max_concurrency\"?",
            (2, 18),
        ),
        (
            "[queues.Emails]\nmax_concurrency = 3\n",
            "invalid value for queues.Emails: invalid queue name \"Emails\": must start with a lowercase letter or digit, got 'E'",
            (1, 1),
        ),
    ];
    let expected: Vec<(String, Option<(usize, usize)>)> = cases
        .iter()
        .map(|(_, message, at)| (message.to_string(), Some(*at)))
        .collect();
    let actual: Vec<(String, Option<(usize, usize)>)> = cases
        .iter()
        .map(|(src, _, _)| {
            let err = error(src);
            (err.message(), err.line_col())
        })
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn diagnostics_render_the_source_line() {
    let err = error("[scheduler]\ntick = \"0s\"\n");
    assert_eq!(
        err.to_string(),
        "error: invalid value for scheduler.tick: must be positive\n \
         --> belay.toml:2:8\n  |\n2 | tick = \"0s\"\n  |        ^^^^"
    );
}

#[test]
fn schedules_are_checked_when_loaded() {
    let base = |cron: &str, tz: &str| {
        format!(
            "[[schedules]]\nid = \"s\"\ncron = \"{cron}\"\ntimezone = \"{tz}\"\n\
             template = {{ queue = \"q\", kind = \"k\" }}\n"
        )
    };
    let err = error(&base("61 * * * *", "UTC"));
    assert!(
        err.message()
            .starts_with("invalid value for schedules[0].cron:"),
        "{}",
        err.message()
    );
    assert!(err.message().contains("61"), "{}", err.message());
    assert_eq!(err.line_col(), Some((3, 8)));

    let err = error(&base("0 * * * *", "Mars/Olympus_Mons"));
    assert!(
        err.message()
            .starts_with("invalid value for schedules[0].timezone:"),
        "{}",
        err.message()
    );

    let twice = format!("{}{}", base("0 * * * *", "UTC"), base("5 * * * *", "UTC"));
    assert_eq!(
        error(&twice).message(),
        "invalid value for schedules[1].id: schedule s is defined twice"
    );

    let err = error(
        "[[schedules]]\nid = \"s\"\ncron = \"0 * * * *\"\n[schedules.template]\nqueue = \"q\"\nkind = \"k\"\npriority = 5000\n",
    );
    assert_eq!(
        err.message(),
        "invalid value for schedules[0].template.priority: must be between -1000 and 1000, got 5000"
    );
    assert_eq!(err.line_col(), Some((7, 12)));
}

#[test]
fn calendars_must_be_unique_and_well_formed() {
    let cal = "[[calendars]]\nname = \"office\"\nwindows = [{ days = [\"mon-fri\"], start = \"09:00\", end = \"17:00\" }]\n";
    let config = load(cal);
    assert_eq!(config.calendars[0].windows.len(), 1);
    let err = error(&format!("{cal}{cal}"));
    assert_eq!(
        err.message(),
        "invalid value for calendars[1].name: calendar \"office\" is defined twice"
    );
    let err = error("[[calendars]]\nname = \"x\"\nexclude_dates = [\"2025-02-30\"]\n");
    assert!(
        err.message()
            .starts_with("invalid value for calendars[0].exclude_dates[0]"),
        "{}",
        err.message()
    );
}

#[test]
fn open_listeners_need_tokens_or_an_explicit_opt_out() {
    let err = error("[server]\nlisten = \"0.0.0.0:7700\"\n");
    assert!(
        err.message().contains("no [[auth.tokens]] are configured"),
        "{}",
        err.message()
    );
    assert_eq!(err.line_col(), Some((2, 10)));
    let config = load("[server]\nlisten = \"0.0.0.0:7700\"\n[auth]\ndisabled = true\n");
    assert!(!config.auth.is_enforced());
}

#[test]
fn a_cluster_behind_a_wildcard_listener_needs_an_api_address() {
    let src =
        "[server]\nlisten = \"0.0.0.0:7700\"\n[auth]\ndisabled = true\n[cluster]\nenabled = true\n";
    let err = error(src);
    assert!(
        err.message().contains("set cluster.api_addr"),
        "{}",
        err.message()
    );
    let fixed = format!("{src}api_addr = \"http://10.1.2.3:7700\"\n");
    assert_eq!(
        load(&fixed).cluster.api_addr.as_deref(),
        Some("http://10.1.2.3:7700")
    );
    let err = error("[cluster]\nnode_id = \"node one\"\n");
    assert!(
        err.message()
            .starts_with("invalid value for cluster.node_id: contains invalid character"),
        "{}",
        err.message()
    );
}

#[test]
fn unknown_keys_are_warnings_with_suggestions() {
    let config = load(
        "[sever]\nlisten = \"x\"\n\n[server]\nthreds = 8\n\n[storage]\nsync = \"never\"\nfsync = true\n",
    );
    let warnings: Vec<(String, Option<String>)> = config
        .warnings()
        .iter()
        .map(|w| (w.path.clone(), w.suggestion.clone()))
        .collect();
    assert_eq!(
        warnings,
        [
            ("sever".to_owned(), Some("server".to_owned())),
            (
                "server.threds".to_owned(),
                Some("server.threads".to_owned())
            ),
            ("storage.fsync".to_owned(), Some("storage.sync".to_owned())),
        ]
    );
    let rendered = config.warnings()[1].to_string();
    assert!(
        rendered.starts_with("warning: unknown key `server.threds`"),
        "{rendered}"
    );
    assert!(
        rendered.contains("did you mean `server.threads`?"),
        "{rendered}"
    );
}

#[test]
fn printed_configuration_loads_back_equal() {
    let original = load(
        "[auth]\n[[auth.tokens]]\nname = \"ci\"\ntoken = \"0123456789abcdef-ci\"\nscopes = [\"enqueue\", \"read\"]\n",
    );
    let example = Config::from_str(Config::example_toml()).unwrap();
    for config in [Config::default(), original, example] {
        let text = config.to_toml();
        let back =
            Config::from_source("printed.toml", &text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(back, config, "{text}");
        assert!(back.warnings().is_empty(), "{:?}", back.warnings());
        assert!(
            !text.contains("0123456789abcdef"),
            "secrets must not be printed"
        );
    }
}

#[test]
fn files_are_read_and_named_in_diagnostics() {
    let dir = belay_common::fs::TempDir::new().unwrap();
    let path = dir.join("belay.toml");
    std::fs::write(&path, "[server]\nthreads = -1\n").unwrap();
    let err = Config::from_file(&path).unwrap_err();
    assert!(err.to_string().contains("belay.toml:2:11"), "{err}");
    let missing = Config::from_file(dir.join("nope.toml")).unwrap_err();
    assert!(
        missing.message().starts_with("cannot read"),
        "{}",
        missing.message()
    );
    assert_eq!(
        Config::load(None).map(|c| c.server.listen).ok(),
        Some(Config::default().server.listen)
            .filter(|_| std::env::var_os("BELAY_SERVER__LISTEN").is_none())
            .or_else(|| Config::load(None).ok().map(|c| c.server.listen))
    );
}
