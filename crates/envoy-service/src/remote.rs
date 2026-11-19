//! High-level remote client backed by any [`AgentDialer`] adapter.

use crate::client::{
    cancel, edit, jobs, list, logs, next_reply, open, probe, read, status, submit, write,
    WriteStream,
};
use crate::transport::{AgentDialer, BoxStream};
use anyhow::{bail, Result};
use envoy_proto::{
    Entry, GrepHit, Hello, Job, JobId, LogRecord, Op, Reply, SubmitRequest, AGENT_SERVICE_ID,
};
use tokio::io::{AsyncRead, AsyncWrite};

pub async fn glob<S>(
    stream: S,
    peer: &str,
    root: &str,
    pattern: &str,
    max_results: u32,
) -> Result<Vec<String>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(
        stream,
        peer,
        Op::Glob {
            root: root.to_string(),
            pattern: pattern.to_string(),
            max_results,
        },
    )
    .await?;
    match next_reply(&mut conn.stream, peer).await? {
        Reply::GlobMatches { paths } => Ok(paths),
        Reply::Done { .. } => Ok(Vec::new()),
        Reply::Failed { error } => bail!(error),
        other => bail!("unexpected reply to a glob: {other:?}"),
    }
}

pub async fn grep<S>(
    stream: S,
    peer: &str,
    root: &str,
    pattern: &str,
    include: Option<&str>,
    max_results: u32,
) -> Result<Vec<GrepHit>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut conn = open(
        stream,
        peer,
        Op::Grep {
            root: root.to_string(),
            pattern: pattern.to_string(),
            include: include.map(str::to_string),
            max_results,
        },
    )
    .await?;
    match next_reply(&mut conn.stream, peer).await? {
        Reply::GrepMatches { matches } => Ok(matches),
        Reply::Done { .. } => Ok(Vec::new()),
        Reply::Failed { error } => bail!(error),
        other => bail!("unexpected reply to a grep: {other:?}"),
    }
}

/// High-level client backed by any [`AgentDialer`] adapter.
pub struct Remote<D> {
    dialer: D,
}

impl<D: AgentDialer> Remote<D> {
    pub fn new(dialer: D) -> Self {
        Self { dialer }
    }

    pub fn dialer(&self) -> &D {
        &self.dialer
    }

    async fn dial(&self, peer: &str) -> Result<BoxStream> {
        self.dialer.open_stream(peer, AGENT_SERVICE_ID).await
    }

    pub async fn probe(&self, peer: &str) -> Result<Hello> {
        probe(self.dial(peer).await?, peer).await
    }

    pub async fn submit(&self, peer: &str, req: SubmitRequest) -> Result<Job> {
        submit(self.dial(peer).await?, peer, req).await
    }

    pub async fn jobs(&self, peer: &str) -> Result<Vec<Job>> {
        jobs(self.dial(peer).await?, peer).await
    }

    pub async fn status(&self, peer: &str, id: JobId) -> Result<Job> {
        status(self.dial(peer).await?, peer, id).await
    }

    pub async fn cancel(&self, peer: &str, id: JobId) -> Result<()> {
        cancel(self.dial(peer).await?, peer, id).await
    }

    pub async fn logs<F>(
        &self,
        peer: &str,
        id: JobId,
        offset: u64,
        follow: bool,
        sink: F,
    ) -> Result<(u64, Option<i32>)>
    where
        F: FnMut(&[LogRecord]),
    {
        logs(self.dial(peer).await?, peer, id, offset, follow, sink).await
    }

    pub async fn read(
        &self,
        peer: &str,
        path: &str,
        offset: u64,
        limit: u64,
    ) -> Result<(u64, u32, Vec<u8>)> {
        read(self.dial(peer).await?, peer, path, offset, limit).await
    }

    pub async fn write(
        &self,
        peer: &str,
        path: &str,
        data: &[u8],
        mode: Option<u32>,
    ) -> Result<u64> {
        write(self.dial(peer).await?, peer, path, data, mode).await
    }

    pub async fn edit(
        &self,
        peer: &str,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        edit(self.dial(peer).await?, peer, path, old, new, replace_all).await
    }

    /// Open a streaming write, for payloads too large to hold in memory.
    pub async fn open_write(
        &self,
        peer: &str,
        path: &str,
        mode: Option<u32>,
    ) -> Result<WriteStream<BoxStream>> {
        let stream = self.dial(peer).await?;
        WriteStream::open(stream, peer, path, mode).await
    }

    pub async fn list(&self, peer: &str, path: &str) -> Result<Vec<Entry>> {
        list(self.dial(peer).await?, peer, path).await
    }

    pub async fn glob(
        &self,
        peer: &str,
        root: &str,
        pattern: &str,
        max_results: u32,
    ) -> Result<Vec<String>> {
        glob(self.dial(peer).await?, peer, root, pattern, max_results).await
    }

    pub async fn grep(
        &self,
        peer: &str,
        root: &str,
        pattern: &str,
        include: Option<&str>,
        max_results: u32,
    ) -> Result<Vec<GrepHit>> {
        grep(
            self.dial(peer).await?,
            peer,
            root,
            pattern,
            include,
            max_results,
        )
        .await
    }
}
