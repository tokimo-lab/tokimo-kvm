//! Minimal QMP client.

use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub struct QmpClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl QmpClient {
    pub async fn connect(path: &Path) -> anyhow::Result<Self> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(path).await {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let (r, w) = stream.into_split();
        let mut me = Self {
            reader: BufReader::new(r),
            writer: w,
        };
        let _ = me.read_message().await?;
        me.execute("qmp_capabilities", None).await?;
        Ok(me)
    }

    async fn read_message(&mut self) -> anyhow::Result<Value> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("QMP closed");
        }
        Ok(serde_json::from_str(&line)?)
    }

    pub async fn execute(&mut self, cmd: &str, args: Option<Value>) -> anyhow::Result<Value> {
        let mut req = serde_json::json!({ "execute": cmd });
        if let Some(a) = args {
            req["arguments"] = a;
        }
        let line = format!("{}\n", req);
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        loop {
            let msg = self.read_message().await?;
            if let Some(r) = msg.get("return") {
                return Ok(r.clone());
            }
            if let Some(e) = msg.get("error") {
                anyhow::bail!("QMP error: {}", e);
            }
        }
    }

    pub async fn powerdown(&mut self) -> anyhow::Result<()> {
        self.execute("system_powerdown", None).await.map(|_| ())
    }
    pub async fn quit(&mut self) -> anyhow::Result<()> {
        let _ = self.execute("quit", None).await;
        Ok(())
    }
}
