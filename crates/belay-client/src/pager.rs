//! Walking a cursor-paginated listing to its end, one page at a time and
//! only as fast as the caller consumes items.

use std::fmt;

use belay_json::FromJson;
use belay_proto::api::{Cursor, Page};

use crate::client::{Call, Client};
use crate::error::{DecodeError, Error, Result};

/// Page size and starting point for a listing.
///
/// ```
/// use belay_client::ListOptions;
///
/// let options = ListOptions::new().limit(50);
/// assert_eq!(options.to_query(), [("limit".to_owned(), "50".to_owned())]);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListOptions {
    /// Continue after this position (from a previous page's
    /// `next_cursor`).
    pub cursor: Option<Cursor>,
    /// Page size; the server applies its default and maximum.
    pub limit: Option<u32>,
}

impl ListOptions {
    /// The first page at the server's default size.
    #[must_use]
    pub fn new() -> ListOptions {
        ListOptions::default()
    }

    /// Continues after `cursor`.
    #[must_use]
    pub fn after(mut self, cursor: Cursor) -> ListOptions {
        self.cursor = Some(cursor);
        self
    }

    /// Sets the page size.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> ListOptions {
        self.limit = Some(limit);
        self
    }

    /// The query parameters, `cursor` then `limit`, unset ones left out.
    #[must_use]
    pub fn to_query(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        if let Some(cursor) = &self.cursor {
            pairs.push(("cursor".to_owned(), cursor.to_string()));
        }
        if let Some(limit) = self.limit {
            pairs.push(("limit".to_owned(), limit.to_string()));
        }
        pairs
    }
}

/// Every item of a listing, fetching the next page when the current one
/// runs out.
///
/// Yields `Err` once and then ends if a page cannot be fetched; items
/// already yielded stay valid. Pages are fetched lazily, so stopping early
/// (`take`, `find`, `break`) costs no further requests. A server that hands
/// back the cursor it was given would loop forever; that is reported as a
/// [`Decode`](Error::Decode) error instead.
///
/// ```no_run
/// use belay_client::Client;
/// use belay_proto::api::JobFilter;
/// use belay_proto::JobState;
///
/// let client = Client::from_env()?;
/// let dead = client.jobs().list_all(JobFilter::new().state(JobState::Dead));
/// for job in dead.take(500) {
///     let job = job?;
///     println!("{} {}", job.id, job.last_error().map_or("", |e| e.message.as_str()));
/// }
/// # Ok::<(), belay_client::Error>(())
/// ```
pub struct Paginator<T> {
    client: Client,
    path: String,
    /// Query parameters of every request, except `cursor`.
    query: Vec<(String, String)>,
    cursor: Option<Cursor>,
    buffered: std::vec::IntoIter<T>,
    finished: bool,
    pages: u32,
}

impl<T: FromJson> Paginator<T> {
    /// A listing of `path`, starting after `cursor`.
    pub(crate) fn new(
        client: Client,
        path: String,
        query: Vec<(String, String)>,
        cursor: Option<Cursor>,
    ) -> Paginator<T> {
        Paginator {
            client,
            path,
            query,
            cursor,
            buffered: Vec::new().into_iter(),
            finished: false,
            pages: 0,
        }
    }

    /// Pages requested so far.
    #[must_use]
    pub fn pages_fetched(&self) -> u32 {
        self.pages
    }

    /// The cursor the next page will be requested with; `None` before the
    /// first page of an unpositioned listing and after the last page.
    #[must_use]
    pub fn cursor(&self) -> Option<&Cursor> {
        self.cursor.as_ref()
    }

    fn fetch_page(&mut self) -> Result<()> {
        let mut query = self.query.clone();
        if let Some(cursor) = &self.cursor {
            query.push(("cursor".to_owned(), cursor.to_string()));
        }
        let call = Call::get(self.path.clone()).query(query);
        let endpoint = call.endpoint();
        let page: Page<T> = self.client.fetch(call)?;
        self.pages += 1;
        match page.next_cursor {
            Some(next) if page.items.is_empty() && self.cursor.as_ref() == Some(&next) => {
                return Err(Error::Decode(DecodeError::new(
                    endpoint,
                    format!("server returned cursor {next} again; stopping instead of looping"),
                )));
            }
            Some(next) => self.cursor = Some(next),
            None => {
                self.cursor = None;
                self.finished = true;
            }
        }
        self.buffered = page.items.into_iter();
        Ok(())
    }
}

impl<T: FromJson> Iterator for Paginator<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Result<T>> {
        loop {
            if let Some(item) = self.buffered.next() {
                return Some(Ok(item));
            }
            if self.finished {
                return None;
            }
            if let Err(e) = self.fetch_page() {
                self.finished = true;
                return Some(Err(e));
            }
        }
    }
}

impl<T> fmt::Debug for Paginator<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Paginator")
            .field("path", &self.path)
            .field("query", &self.query)
            .field("cursor", &self.cursor.as_ref().map(ToString::to_string))
            .field("pages", &self.pages)
            .field("buffered", &self.buffered.len())
            .field("finished", &self.finished)
            .finish()
    }
}
