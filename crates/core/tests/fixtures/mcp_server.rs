//! Minimal MCP stdio server for live tests — newline-delimited JSON-RPC
//! 2.0 (the framing rmcp's child-process transport uses).
//!
//! Contract:
//!   initialize          → protocolVersion + capabilities.tools + serverInfo
//!   notifications/*     → ignored (no reply)
//!   tools/list          → one "ping" tool
//!   tools/call ping     → text content echoing arguments.msg
//!   anything else       → method-not-found error
//!
//! Flags:
//!   --die    reply to initialize + tools/list, then exit(0) — every later
//!            request hits a dead child (the crash-tolerance path)
//!   --hang   reply to initialize, then sleep forever without another byte
//!
//! Protocol notes that keep this honest:
//!   - requests carry a numeric-or-string "id"; the reply must echo it
//!   - notifications have no "id" — replying to one is a protocol bug
//!   - results stay JSON-RPC shaped; MCP wraps them in `result.content`

use std::io::{BufRead, BufReader, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let die = args.iter().any(|a| a == "--die");
    let hang = args.iter().any(|a| a == "--hang");

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut saw_list = false;

    for line in BufReader::new(stdin.lock()).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json_lite::parse(&line) else {
            continue;
        };
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = v.get("id");
        if method.starts_with("notifications/") || method.is_empty() {
            continue; // notifications never get a reply
        }
        let Some(id) = id else { continue };
        let result = match method {
            "initialize" => Some(format!(
                "{{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"mcp-echo\",\"version\":\"0.1\"}}}}"
            )),
            "tools/list" => {
                saw_list = true;
                Some("{\"tools\":[{\"name\":\"ping\",\"description\":\"echo the msg\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"msg\":{\"type\":\"string\"}}}}]}".to_string())
            }
            "tools/call" => {
                let name = v
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let msg = v
                    .get("params")
                    .and_then(|p| p.get("arguments"))
                    .and_then(|a| a.get("msg"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                if name == "ping" {
                    Some(format!(
                        "{{\"content\":[{{\"type\":\"text\",\"text\":\"pong: {msg}\"}}],\"isError\":false}}"
                    ))
                } else {
                    Some(format!(
                        "{{\"content\":[{{\"type\":\"text\",\"text\":\"unknown tool {name}\"}}],\"isError\":true}}"
                    ))
                }
            }
            _ => None, // method-not-found
        };
        let frame = match result {
            Some(r) => format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{r}}}"),
            None => format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"method not found\"}}}}"
            ),
        };
        let _ = writeln!(out, "{frame}");
        let _ = out.flush();
        if die && saw_list && method == "tools/list" {
            std::process::exit(0);
        }
    }
    if hang {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }
}

/// A five-field JSON reader — enough for the MCP handshake without
/// pulling serde into a fixture rustc must compile standalone.
mod serde_json_lite {
    #[derive(Debug)]
    pub enum V {
        Obj(Vec<(String, V)>),
        Arr(Vec<V>),
        Str(String),
        Num(f64),
        Bool(bool),
        Null,
    }

    impl V {
        pub fn get(&self, key: &str) -> Option<&V> {
            match self {
                V::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }
        pub fn as_str(&self) -> Option<&str> {
            match self {
                V::Str(s) => Some(s),
                _ => None,
            }
        }
    }

    impl std::fmt::Display for V {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                V::Str(s) => write!(f, "\"{s}\""),
                V::Num(n) => {
                    if n.fract() == 0.0 {
                        write!(f, "{}", *n as i64)
                    } else {
                        write!(f, "{n}")
                    }
                }
                V::Bool(b) => write!(f, "{b}"),
                V::Null => write!(f, "null"),
                V::Arr(xs) => {
                    write!(f, "[")?;
                    for (i, x) in xs.iter().enumerate() {
                        if i > 0 {
                            write!(f, ",")?;
                        }
                        write!(f, "{x}")?;
                    }
                    write!(f, "]")
                }
                V::Obj(kv) => {
                    write!(f, "{{")?;
                    for (i, (k, v)) in kv.iter().enumerate() {
                        if i > 0 {
                            write!(f, ",")?;
                        }
                        write!(f, "\"{k}\":{v}")?;
                    }
                    write!(f, "}}")
                }
            }
        }
    }

    pub fn parse(s: &str) -> Result<V, ()> {
        let mut p = Parser {
            b: s.as_bytes(),
            i: 0,
        };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i == p.b.len() {
            Ok(v)
        } else {
            Err(())
        }
    }

    struct Parser<'a> {
        b: &'a [u8],
        i: usize,
    }

    impl<'a> Parser<'a> {
        fn ws(&mut self) {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
        }
        fn peek(&self) -> Option<u8> {
            self.b.get(self.i).copied()
        }
        fn value(&mut self) -> Result<V, ()> {
            match self.peek().ok_or(())? {
                b'{' => self.obj(),
                b'[' => self.arr(),
                b'"' => Ok(V::Str(self.string()?)),
                b't' => self.lit("true").map(|_| V::Bool(true)),
                b'f' => self.lit("false").map(|_| V::Bool(false)),
                b'n' => self.lit("null").map(|_| V::Null),
                _ => self.number(),
            }
        }
        fn lit(&mut self, w: &str) -> Result<(), ()> {
            if self.b[self.i..].starts_with(w.as_bytes()) {
                self.i += w.len();
                Ok(())
            } else {
                Err(())
            }
        }
        fn string(&mut self) -> Result<String, ()> {
            if self.peek() != Some(b'"') {
                return Err(());
            }
            self.i += 1;
            let mut s = String::new();
            while let Some(c) = self.peek() {
                self.i += 1;
                match c {
                    b'"' => return Ok(s),
                    b'\\' => {
                        let e = self.peek().ok_or(())?;
                        self.i += 1;
                        s.push(match e {
                            b'n' => '\n',
                            b't' => '\t',
                            b'r' => '\r',
                            b'u' => {
                                // \uXXXX — keep BMP only
                                let h = std::str::from_utf8(&self.b[self.i..self.i + 4])
                                    .map_err(|_| ())?;
                                let cp = u32::from_str_radix(h, 16).map_err(|_| ())?;
                                self.i += 4;
                                char::from_u32(cp).unwrap_or('?')
                            }
                            c => c as char,
                        });
                    }
                    _ => {
                        // multi-byte utf8: find char boundary length
                        let len = utf8_len(c);
                        let chunk = &self.b[self.i - 1..self.i - 1 + len];
                        s.push_str(std::str::from_utf8(chunk).map_err(|_| ())?);
                        self.i += len - 1;
                    }
                }
            }
            Err(())
        }
        fn number(&mut self) -> Result<V, ()> {
            let start = self.i;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                    self.i += 1;
                } else {
                    break;
                }
            }
            let t = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| ())?;
            t.parse::<f64>().map(V::Num).map_err(|_| ())
        }
        fn obj(&mut self) -> Result<V, ()> {
            self.i += 1; // {
            let mut kv = Vec::new();
            self.ws();
            if self.peek() == Some(b'}') {
                self.i += 1;
                return Ok(V::Obj(kv));
            }
            loop {
                self.ws();
                let k = self.string()?;
                self.ws();
                if self.peek() != Some(b':') {
                    return Err(());
                }
                self.i += 1;
                self.ws();
                let v = self.value()?;
                kv.push((k, v));
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b'}') => {
                        self.i += 1;
                        return Ok(V::Obj(kv));
                    }
                    _ => return Err(()),
                }
            }
        }
        fn arr(&mut self) -> Result<V, ()> {
            self.i += 1; // [
            let mut xs = Vec::new();
            self.ws();
            if self.peek() == Some(b']') {
                self.i += 1;
                return Ok(V::Arr(xs));
            }
            loop {
                self.ws();
                xs.push(self.value()?);
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b']') => {
                        self.i += 1;
                        return Ok(V::Arr(xs));
                    }
                    _ => return Err(()),
                }
            }
        }
    }

    fn utf8_len(b: u8) -> usize {
        if b < 0x80 {
            1
        } else if b >> 5 == 0b110 {
            2
        } else if b >> 4 == 0b1110 {
            3
        } else {
            4
        }
    }
}
