//! Search RPC handlers (`glob` and `grep`) for [`crate::server::AgentServer`].

use crate::server::reply;
use anyhow::Result;
use envoy_core::search;
use envoy_proto::Reply;
use tokio::io::AsyncWrite;

pub(crate) async fn glob_op<W: AsyncWrite + Unpin>(
    root: &str,
    pattern: &str,
    max_results: u32,
    wr: &mut W,
) -> Result<()> {
    match search::glob_files(root, pattern, max_results).await {
        Ok(paths) => {
            reply(wr, Reply::GlobMatches { paths }).await?;
            reply(wr, Reply::Done { code: 0, ms: 0 }).await?;
        }
        Err(e) => {
            reply(
                wr,
                Reply::Failed {
                    error: format!("{e:#}"),
                },
            )
            .await?;
        }
    }
    Ok(())
}

pub(crate) async fn grep_op<W: AsyncWrite + Unpin>(
    root: &str,
    pattern: &str,
    include: Option<&str>,
    max_results: u32,
    wr: &mut W,
) -> Result<()> {
    match search::grep_files(root, pattern, include, max_results).await {
        Ok(matches) => {
            reply(wr, Reply::GrepMatches { matches }).await?;
            reply(wr, Reply::Done { code: 0, ms: 0 }).await?;
        }
        Err(e) => {
            reply(
                wr,
                Reply::Failed {
                    error: format!("{e:#}"),
                },
            )
            .await?;
        }
    }
    Ok(())
}
