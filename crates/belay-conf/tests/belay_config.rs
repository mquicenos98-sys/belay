//! Loads a realistic belay configuration the way the engine does: parse,
//! overlay the environment, extract typed values through a `Reader`, then
//! warn about keys nobody read.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use belay_conf::{Document, Error, Origin, Overlay, Reader, Result, Value};

const BELAY_TOML: &str = r#"# belay.toml - production node eu-1
[server]
listen = "0.0.0.0:7700"
threads = 16
max_body = 8_388_608   # 8 MiB

[storage]
dir = '/var/lib/belay'
in_memory = false
sync = "interval:10ms"
checkpoint_bytes = 0x4000_0000

[auth]
tokens = [
  { name = "ops", token_sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08", scopes = ["admin"] },  # gitleaks:allow
  { name = "ci", token = "ci-secret", scopes = ["enqueue", "read"] },
]

[log]
level = "info"
format = "logfmt"

[queues.emails]
max_concurrency = 8
rate_limit = { per_second = 50.0, burst = 100 }
default_timeout = "30s"

[queues."reports.daily"]
max_concurrency = 1
default_retry = "exp:1s*2..10m"

[[schedules]]
id = "nightly-report"
cron = "0 3 * * *"
timezone = "Europe/Berlin"
starts_at = 2025-01-01T00:00:00Z
template = { queue = "reports.daily", kind = "report.build", payload = { format = "pdf" } }

[[schedules]]
id = "heartbeat"
cron = "*/5 * * * *"
misfire = "skip"
jitter = "10s"

  [schedules.template]
  queue = "emails"
  kind = "ping"
  meta = { team = "platform" }

[scheduler]
tick = "250ms"

[retention]
succeeded = "7d"
dead = "30d"

[cluster]
node_id = "eu-1"
gossip_listen = "0.0.0.0:7946"
seeds = ["10.0.0.2:7946", "10.0.0.3:7946"]

[metrics]
enabled = true
"#;

#[derive(Debug, PartialEq)]
struct Token {
    name: String,
    secret: String,
    scopes: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct Queue {
    name: String,
    max_concurrency: Option<u32>,
    rate_limit: Option<(f64, u32)>,
    default_timeout: Option<String>,
}

#[derive(Debug, PartialEq)]
struct Schedule {
    id: String,
    cron: String,
    timezone: String,
    starts_at: Option<String>,
    queue: String,
    kind: String,
    meta: BTreeMap<String, String>,
}

#[derive(Debug)]
struct Config {
    listen: SocketAddr,
    threads: u32,
    max_body: u64,
    dir: String,
    sync: String,
    checkpoint_bytes: u64,
    tokens: Vec<Token>,
    log_level: String,
    queues: Vec<Queue>,
    schedules: Vec<Schedule>,
    seeds: Vec<String>,
    metrics: bool,
}

/// What belay-core's loader looks like, reduced to a representative subset.
fn load(doc: &Document) -> Result<(Config, Reader<'_>)> {
    let r = Reader::new(doc);
    let listen = r
        .opt_parsed("server.listen")?
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 7700)));
    let threads = r.or("server.threads", 4)?;
    if threads == 0 {
        return Err(r.invalid("server.threads", "must be at least 1"));
    }
    let mut tokens = Vec::new();
    for t in r.array_of_tables("auth.tokens")? {
        let secret = match (t.opt_string("token")?, t.opt_string("token_sha256")?) {
            (Some(s), None) | (None, Some(s)) => s,
            _ => return Err(t.invalid("name", "set exactly one of `token` and `token_sha256`")),
        };
        tokens.push(Token {
            name: t.string("name")?,
            secret,
            scopes: t.req("scopes")?,
        });
    }
    let mut queues = Vec::new();
    for (name, q) in r.tables("queues")? {
        let rate_limit = match q.table("rate_limit")? {
            Some(rl) => Some((rl.req("per_second")?, rl.req("burst")?)),
            None => None,
        };
        q.opt_string("default_retry")?;
        queues.push(Queue {
            name,
            max_concurrency: q.opt("max_concurrency")?,
            rate_limit,
            default_timeout: q.opt_string("default_timeout")?,
        });
    }
    let mut schedules = Vec::new();
    for s in r.array_of_tables("schedules")? {
        for key in ["misfire", "jitter"] {
            s.opt_string(key)?;
        }
        let template = s
            .table("template")?
            .ok_or_else(|| s.invalid("id", "schedule has no [template]"))?;
        template.opt::<Value>("payload")?;
        schedules.push(Schedule {
            id: s.string("id")?,
            cron: s.string("cron")?,
            timezone: s.string_or("timezone", "UTC")?,
            starts_at: s.opt_string("starts_at")?,
            queue: template.string("queue")?,
            kind: template.string("kind")?,
            meta: template.or("meta", BTreeMap::new())?,
        });
    }
    for key in [
        "log.format",
        "scheduler.tick",
        "retention.succeeded",
        "retention.dead",
    ] {
        r.opt_string(key)?;
    }
    for key in [
        "cluster.node_id",
        "cluster.gossip_listen",
        "cluster.advertise",
    ] {
        r.opt_string(key)?;
    }
    let config = Config {
        listen,
        threads,
        max_body: r.or("server.max_body", 1 << 20)?,
        dir: r.string_or("storage.dir", "./data")?,
        sync: r.string_or("storage.sync", "always")?,
        checkpoint_bytes: r.or("storage.checkpoint_bytes", 64 << 20)?,
        tokens,
        log_level: r.string_or("log.level", "info")?,
        queues,
        schedules,
        seeds: r.or("cluster.seeds", Vec::new())?,
        metrics: r.or("metrics.enabled", false)?,
    };
    r.or("storage.in_memory", false)?;
    Ok((config, r))
}

#[test]
fn loads_every_section_of_a_production_config() {
    let doc = Document::parse_named("belay.toml", BELAY_TOML).unwrap();
    let (c, r) = load(&doc).unwrap();
    assert_eq!(c.listen, "0.0.0.0:7700".parse().unwrap());
    assert_eq!((c.threads, c.max_body), (16, 8 << 20));
    assert_eq!(
        (c.dir.as_str(), c.sync.as_str()),
        ("/var/lib/belay", "interval:10ms")
    );
    assert_eq!(c.checkpoint_bytes, 1 << 30);
    assert_eq!(c.log_level, "info");
    assert_eq!(c.tokens.len(), 2);
    assert_eq!(
        c.tokens[1],
        Token {
            name: "ci".into(),
            secret: "ci-secret".into(),
            scopes: vec!["enqueue".into(), "read".into()],
        }
    );
    assert_eq!(c.queues.len(), 2);
    assert_eq!(c.queues[0].rate_limit, Some((50.0, 100)));
    assert_eq!(c.queues[1].name, "reports.daily");
    assert_eq!(c.queues[1].max_concurrency, Some(1));
    assert_eq!(
        c.schedules[0].starts_at.as_deref(),
        Some("2025-01-01T00:00:00Z")
    );
    assert_eq!(c.schedules[0].queue, "reports.daily");
    assert_eq!(c.schedules[1].timezone, "UTC");
    assert_eq!(
        c.schedules[1].meta.get("team").map(String::as_str),
        Some("platform")
    );
    assert_eq!(c.seeds, ["10.0.0.2:7946", "10.0.0.3:7946"]);
    assert!(c.metrics);
    assert_eq!(r.unused_keys(), []);
}

#[test]
fn typos_are_reported_with_location_and_suggestion() {
    let src = BELAY_TOML
        .replace("[metrics]", "[metrcs]")
        .replace("max_concurrency = 1\n", "max_concurency = 1\n")
        .replace("jitter = \"10s\"", "jiter = \"10s\"");
    let doc = Document::parse(&src).unwrap();
    let (_, r) = load(&doc).unwrap();
    let unused = r.unused_keys();
    let summary: Vec<(&str, Option<&str>)> = unused
        .iter()
        .map(|u| (u.path.as_str(), u.suggestion.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "queues.\"reports.daily\".max_concurency",
                Some("queues.\"reports.daily\".max_concurrency")
            ),
            ("schedules[1].jiter", Some("schedules[1].jitter")),
            ("metrcs", Some("metrics")),
        ]
    );
    let line = |needle: &str| src.lines().position(|l| l.contains(needle)).unwrap() + 1;
    assert_eq!(
        unused[2].to_string(),
        format!(
            "warning: unknown key `metrcs`\n  --> line {n}, column 2\n   |\n{n} | [metrcs]\n   |  ^^^^^^\n   \
             = help: did you mean `metrics`?",
            n = line("[metrcs]")
        )
    );
    assert!(matches!(r.deny_unused(), Err(Error::Unknown(_))));
}

#[test]
fn environment_overrides_win_and_are_blamed_in_errors() {
    let mut doc = Document::parse_named("belay.toml", BELAY_TOML).unwrap();
    let vars = [
        ("BELAY_SERVER__THREADS", "32"),
        ("BELAY_STORAGE__IN_MEMORY", "true"),
        ("BELAY_QUEUES__EMAILS__MAX_CONCURRENCY", "2"),
        ("BELAY_CLUSTER__SEEDS", r#"["10.0.0.9:7946"]"#),
        ("BELAY_TOKEN", "not-configuration"),
        ("PATH", "/usr/bin"),
    ];
    let applied = Overlay::new("BELAY").apply(&mut doc, vars).unwrap();
    assert_eq!(applied.len(), 4);
    let (c, _) = load(&doc).unwrap();
    assert_eq!(c.threads, 32);
    assert_eq!(c.queues[0].max_concurrency, Some(2));
    assert_eq!(c.seeds, ["10.0.0.9:7946"]);
    assert_eq!(doc.get("storage.in_memory"), Some(&Value::Boolean(true)));

    Overlay::new("BELAY")
        .apply(&mut doc, [("BELAY_SERVER__THREADS", "0")])
        .unwrap();
    let err = load(&doc).unwrap_err();
    assert_eq!(
        err.to_string(),
        "error: invalid value for server.threads: must be at least 1\n \
         --> environment variable BELAY_SERVER__THREADS"
    );
}

#[test]
fn domain_parse_failures_point_at_the_value() {
    let src = BELAY_TOML.replace("0.0.0.0:7700", "0.0.0.0:77000");
    let doc = Document::parse_named("belay.toml", &src).unwrap();
    let err = load(&doc).unwrap_err();
    assert_eq!(
        err.to_string(),
        "error: invalid value for server.listen: invalid socket address syntax\n \
         --> belay.toml:3:10\n  |\n3 | listen = \"0.0.0.0:77000\"\n  |          ^^^^^^^^^^^^^^^"
    );
}

#[test]
fn type_and_consistency_errors_inside_arrays_of_tables() {
    let src = BELAY_TOML.replace(
        "scopes = [\"enqueue\", \"read\"]",
        "scopes = [\"enqueue\", 7]",
    );
    let err = load(&Document::parse(&src).unwrap()).unwrap_err();
    assert_eq!(
        err.message(),
        "expected string at auth.tokens[1].scopes[1], found integer"
    );
    assert_eq!(err.line_col(), Some((16, 60)));

    let src = BELAY_TOML.replace(
        "token = \"ci-secret\"",
        "token = \"x\", token_sha256 = \"y\"",
    );
    let err = load(&Document::parse(&src).unwrap()).unwrap_err();
    assert_eq!(
        err.message(),
        "invalid value for auth.tokens[1].name: set exactly one of `token` and `token_sha256`"
    );
    assert_eq!(err.line_col(), Some((16, 12)));

    let src = BELAY_TOML.replace("cron = \"*/5 * * * *\"\n", "");
    let err = load(&Document::parse(&src).unwrap()).unwrap_err();
    assert_eq!(err.message(), "missing required key `schedules[1].cron`");
    assert!(
        matches!(err, Error::Missing { table: Some(Origin::Source(ref s)), .. } if s.text == "[[schedules]]")
    );
}

#[test]
fn printed_config_parses_back_to_the_same_values() {
    let doc = Document::parse(BELAY_TOML).unwrap();
    let printed = doc.to_toml();
    let again = Document::parse(&printed).unwrap();
    assert_eq!(again.root(), doc.root());
    assert_eq!(again.to_toml(), printed, "printing is stable");
    assert!(printed
        .starts_with("[server]\nlisten = \"0.0.0.0:7700\"\nthreads = 16\nmax_body = 8388608\n"));
    assert!(printed.contains("\n[queues.\"reports.daily\"]\n"));
    assert!(printed.contains("\n[[auth.tokens]]\nname = \"ops\"\n"));
    assert!(printed.contains("\n[schedules.template]\nqueue = \"emails\"\n"));

    // Redacting secrets before printing, as `belay config print` does.
    let mut redacted = doc.clone();
    redacted.set("auth.tokens[1].token", "<redacted>").unwrap();
    assert!(!redacted.to_toml().contains("ci-secret"));
}

#[test]
fn reads_from_disk_with_file_names_in_diagnostics() {
    let dir = std::env::temp_dir().join(format!("belay-conf-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("belay.toml");
    std::fs::write(&path, BELAY_TOML.replace('\n', "\r\n")).unwrap();
    let doc = Document::from_file(&path).unwrap();
    let (c, _) = load(&doc).unwrap();
    assert_eq!(c.schedules.len(), 2);
    assert_eq!(doc.name(), Some(path.display().to_string().as_str()));

    let broken = dir.join("broken.toml");
    std::fs::write(&broken, "[server]\nthreads = [1,\n").unwrap();
    let err = Document::from_file(&broken).unwrap_err();
    assert!(err.to_string().contains("broken.toml:2:11\n"), "{err}");
    let missing = Document::from_file(dir.join("nope.toml")).unwrap_err();
    assert!(matches!(missing, Error::Io { .. }));
    std::fs::remove_dir_all(&dir).unwrap();
}
