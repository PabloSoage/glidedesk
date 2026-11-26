//! The little HTTP/1.1 GameStream needs: one GET (or small POST) per connection, answered
//! with XML.

use std::collections::HashMap;

use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn arg(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }
}

pub async fn read_request<S: AsyncRead + Unpin>(s: &mut S) -> Result<Request> {
    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        let mut chunk = [0u8; 4096];
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed before the request");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        // Client certificates go hex-encoded in the query of `getservercert`.
        if buf.len() > 64 << 10 {
            bail!("request head too large");
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_owned();
    let target = first.next().unwrap_or_default();
    let length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    if length > 64 << 10 {
        bail!("request body too large");
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let mut chunk = vec![0u8; length - body.len()];
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed in the body");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Request {
        method,
        path: path.to_owned(),
        query: parse_query(query),
        body,
    })
}

/// `a=1&b=x%20y` → map (later keys win).
pub fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let digits = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(digits, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An XML answer: `<root status_code=...>` with simple child elements.
pub struct Xml {
    status: u16,
    message: Option<String>,
    body: String,
}

impl Xml {
    pub fn ok() -> Self {
        Self::status(200)
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            message: None,
            body: String::new(),
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: Some(message.into()),
            body: String::new(),
        }
    }

    pub fn put(mut self, name: &str, value: impl std::fmt::Display) -> Self {
        self.body
            .push_str(&format!("<{name}>{}</{name}>", escape(&value.to_string())));
        self
    }

    /// A nested element with its own children (`App` in `/applist`).
    pub fn child(mut self, name: &str, fields: &[(&str, String)]) -> Self {
        self.body.push_str(&format!("<{name}>"));
        for (k, v) in fields {
            self.body.push_str(&format!("<{k}>{}</{k}>", escape(v)));
        }
        self.body.push_str(&format!("</{name}>"));
        self
    }

    pub fn render(&self) -> String {
        let msg = self
            .message
            .as_ref()
            .map(|m| format!(" status_message=\"{}\"", escape(m)))
            .unwrap_or_default();
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"{}\"{msg}>{}</root>",
            self.status, self.body
        )
    }
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub async fn respond<S: AsyncWrite + Unpin>(
    s: &mut S,
    status: &str,
    kind: &str,
    body: &[u8],
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(body).await?;
    s.shutdown().await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_decoded() {
        let q = parse_query("devicename=roth&salt=AB%2Fcd&x=a+b&flag");
        assert_eq!(q["devicename"], "roth");
        assert_eq!(q["salt"], "AB/cd");
        assert_eq!(q["x"], "a b");
        assert_eq!(q["flag"], "");
        assert_eq!(parse_query("a=%zz%4")["a"], "%zz%4");
    }

    #[test]
    fn xml_is_escaped() {
        let x = Xml::ok().put("hostname", "a<b&c").render();
        assert!(x.contains("<root status_code=\"200\"><hostname>a&lt;b&amp;c</hostname></root>"));
        let e = Xml::error(401, "no \"cert\"").render();
        assert!(e.contains("status_code=\"401\" status_message=\"no &quot;cert&quot;\""));
    }
}
