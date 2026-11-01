//! Pump a child's stdout and stderr into timestamped log batches.

use std::time::Duration;

use repoman_common::clock;
use repoman_common::proto::{Fd, LogLine};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;

const BATCH_LINES: usize = 64;
const BATCH_DELAY: Duration = Duration::from_millis(250);
const MAX_LINE: usize = 16 * 1024;

/// Read lines from `reader`, tagging them with `fd`, into `tx`.
pub async fn pump<R: AsyncRead + Unpin>(reader: R, fd: Fd, tx: mpsc::UnboundedSender<LogLine>) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(mut line)) => {
                if line.len() > MAX_LINE {
                    line.truncate(MAX_LINE);
                    line.push_str(" [truncated]");
                }
                if tx
                    .send(LogLine {
                        ts: clock::now(),
                        fd,
                        line,
                        seq: 0,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) => return,
            Err(e) => {
                let _ = tx.send(LogLine {
                    ts: clock::now(),
                    fd: Fd::System,
                    line: format!("log read error on {}: {e}", fd.as_str()),
                    seq: 0,
                });
                return;
            }
        }
    }
}

/// Collect lines into batches and hand them to `emit`. Also keeps the tail
/// for error reporting. Returns when the sender side is closed.
pub async fn batch(mut rx: mpsc::UnboundedReceiver<LogLine>, mut emit: impl FnMut(Vec<LogLine>)) -> Vec<String> {
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::with_capacity(40);
    let mut buf: Vec<LogLine> = Vec::new();
    loop {
        let first = match rx.recv().await {
            Some(l) => l,
            None => break,
        };
        buf.push(first);
        let deadline = tokio::time::sleep(BATCH_DELAY);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                more = rx.recv() => match more {
                    Some(l) => {
                        buf.push(l);
                        if buf.len() >= BATCH_LINES { break; }
                    }
                    None => break,
                },
                _ = &mut deadline => break,
            }
        }
        for l in &buf {
            if tail.len() == 40 {
                tail.pop_front();
            }
            tail.push_back(l.line.clone());
        }
        emit(std::mem::take(&mut buf));
    }
    tail.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pumps_and_batches_in_order() {
        let (tx, rx) = mpsc::unbounded_channel();
        let data = b"one\ntwo\nthree\n".to_vec();
        tokio::spawn(pump(std::io::Cursor::new(data), Fd::Stdout, tx));
        let mut got: Vec<String> = Vec::new();
        let tail = batch(rx, |lines| {
            got.extend(lines.into_iter().map(|l| l.line));
        })
        .await;
        assert_eq!(got, ["one", "two", "three"]);
        assert_eq!(tail, ["one", "two", "three"]);
    }
}
