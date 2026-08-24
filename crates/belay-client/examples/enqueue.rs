//! Enqueues a job on a belay server over HTTP and waits for its outcome.
//!
//! The server is taken from `BELAY_URL` (default `http://127.0.0.1:7700`)
//! and the token from `BELAY_TOKEN`. With `belay dev` running, which
//! serves the `default` queue with an `echo` handler:
//!
//! ```text
//! cargo run -p belay-client --example enqueue
//! cargo run -p belay-client --example enqueue -- default echo '{"hello": "world"}'
//! ```
//!
//! Arguments: queue, kind and a JSON payload, all optional.

use std::error::Error;
use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Client, ClientConfig};
use belay_proto::api::EnqueueStatus;
use belay_proto::{JobSpec, QueueName, UniqueScope};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let queue: QueueName = args.next().as_deref().unwrap_or("default").parse()?;
    let kind = args.next().unwrap_or_else(|| "echo".to_owned());
    let payload = match args.next() {
        Some(text) => belay_json::parse(&text)?,
        None => belay_json::json!({"greeting": "hello from belay-client"}),
    };

    let client = Client::new(ClientConfig::from_env().timeout(Duration::from_secs(10)))?;
    let info = client.info()?;
    println!(
        "connected to {} (belay {}, node {})",
        client.base_url(),
        info.version,
        info.node
    );

    // A uniqueness key makes the enqueue safe to retry: if the response
    // is lost and the client sends it again, the server answers with the
    // job it already has instead of creating a second one.
    let key = format!("example:{}", std::process::id());
    let spec = JobSpec::new(queue, kind)
        .payload(payload)
        .unique(key, UniqueScope::For(Duration::from_secs(600)))
        .meta("source", "belay-client example");
    let created = client.jobs().enqueue(&spec)?;
    match created.status {
        EnqueueStatus::Duplicate => println!("job {} already existed", created.id),
        _ => println!("enqueued {} ({})", created.id, created.state),
    }

    // Poll until a worker has finished it. (`belay events tail` or
    // `Client::events` would say so without polling.)
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let job = client.jobs().get(created.id)?;
        if job.is_terminal() {
            println!(
                "job {} {} after {} attempt(s)",
                job.id, job.state, job.attempt
            );
            if let Some(result) = &job.result {
                println!("result: {}", belay_json::to_string_pretty(result));
            }
            if let Some(error) = job.last_error() {
                println!("last error: {}", error.message);
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            println!(
                "job {} is still {}; is a worker serving queue {}?",
                job.id, job.state, job.spec.queue
            );
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
}
