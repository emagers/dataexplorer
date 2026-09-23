use crate::config::LanguageServer;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};
use unicode_width::UnicodeWidthChar;

pub const URI: &str = "file:///dataexplorer/query.kql";
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

pub fn utf16_to_byte(line: &str, offset: usize) -> Option<usize> {
    let mut utf16 = 0;
    for (byte, c) in line.char_indices() {
        if utf16 == offset {
            return Some(byte);
        }
        utf16 += c.len_utf16();
    }
    (utf16 == offset).then_some(line.len())
}
pub fn char_to_utf16(line: &str, chars: usize) -> usize {
    line.chars().take(chars).map(char::len_utf16).sum()
}
pub fn byte_to_display(line: &str, byte: usize) -> Option<usize> {
    if !line.is_char_boundary(byte) {
        return None;
    }
    let mut col = 0;
    for c in line[..byte].chars() {
        col += if c == '\t' {
            4 - col % 4
        } else {
            c.width().unwrap_or(0)
        };
    }
    Some(col)
}

pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value> {
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        let n = (&mut *reader)
            .take((8193 - header_bytes) as u64)
            .read_line(&mut line)
            .await
            .context("reading LSP header")?;
        ensure!(n != 0, "language server closed stdout");
        header_bytes += n;
        ensure!(header_bytes <= 8192, "LSP header too large");
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, val)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("Content-Length") {
                ensure!(length.is_none(), "duplicate LSP Content-Length");
                length = Some(
                    val.trim()
                        .parse::<usize>()
                        .context("invalid LSP Content-Length")?,
                );
            }
        } else {
            bail!("malformed LSP header");
        }
    }
    let length = length.context("missing LSP Content-Length")?;
    ensure!(
        length <= MAX_MESSAGE,
        "LSP message exceeds 8 MiB safety limit"
    );
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .context("incomplete LSP message")?;
    serde_json::from_slice(&bytes).context("invalid LSP JSON")
}
pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_MESSAGE, "LSP document exceeds 8 MiB");
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
#[derive(Debug)]
pub enum Event {
    Status(String),
    Ready,
    Tokens {
        version: i64,
        data: Vec<u32>,
        legend: Vec<String>,
    },
    Diagnostics {
        version: i64,
        diagnostics: Value,
    },
    Completion {
        version: i64,
        value: Value,
    },
    Hover {
        version: i64,
        value: Value,
    },
}
enum Action {
    Document {
        version: i64,
        text: String,
    },
    Schema(Value),
    Completion {
        version: i64,
        line: usize,
        character: usize,
    },
    Hover {
        version: i64,
        line: usize,
        character: usize,
    },
    Shutdown,
}
pub struct Handle {
    tx: mpsc::Sender<Action>,
    pub events: mpsc::Receiver<Event>,
    task: tokio::task::JoinHandle<()>,
}
impl Handle {
    pub fn start(config: LanguageServer) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let (events_tx, events) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            if let Err(e) = serve(config, rx, events_tx.clone()).await {
                let _ = events_tx
                    .send(Event::Status(format!(
                        "language service unavailable: {e:#}; editor remains usable"
                    )))
                    .await;
            }
        });
        Self { tx, events, task }
    }
    pub fn document(&self, version: i64, text: String) -> Result<()> {
        self.tx
            .try_send(Action::Document { version, text })
            .context("language server busy or offline")
    }
    pub fn schema(&self, schema: Value) -> Result<()> {
        self.tx
            .try_send(Action::Schema(schema))
            .context("language server busy or offline")
    }
    pub fn completion(&self, version: i64, line: usize, character: usize) -> Result<()> {
        self.tx
            .try_send(Action::Completion {
                version,
                line,
                character,
            })
            .context("language server busy or offline")
    }
    pub fn hover(&self, version: i64, line: usize, character: usize) -> Result<()> {
        self.tx
            .try_send(Action::Hover {
                version,
                line,
                character,
            })
            .context("language server busy or offline")
    }
    pub async fn shutdown(mut self) {
        let _ = self.tx.try_send(Action::Shutdown);
        if tokio::time::timeout(Duration::from_secs(3), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}
#[derive(Clone, Copy)]
enum Request {
    Tokens,
    Completion,
    Hover,
}

async fn serve(
    config: LanguageServer,
    mut actions: mpsc::Receiver<Action>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let mut child = Command::new(&config.command)
        .args(&config.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot launch {}", config.command))?;
    let mut input = child.stdin.take().context("LSP stdin missing")?;
    let mut output = BufReader::new(child.stdout.take().context("LSP stdout missing")?);
    let stderr = child.stderr.take().context("LSP stderr missing")?;
    let stderr_events = events.clone();
    let stderr_task = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buf = [0; 1024];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            // Content is deliberately suppressed: external servers may print document data.
            let _ = stderr_events.try_send(Event::Status(
                "language server wrote to stderr (content suppressed)".into(),
            ));
        }
    });
    let result = async {
        write_message(&mut input, &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "processId":std::process::id(),"rootUri":null,
            "capabilities":{"general":{"positionEncodings":["utf-16"]},"textDocument":{"synchronization":{"didSave":false},"publishDiagnostics":{"versionSupport":true},"semanticTokens":{"requests":{"full":true},"tokenTypes":["comment","keyword","string","number","type","function","parameter","variable","property","class","namespace","operator"],"tokenModifiers":[],"formats":["relative"]},"completion":{"completionItem":{"snippetSupport":false}}}}
        }})).await?;
        let init = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let value = read_message(&mut output).await?;
                if value["id"] == 1 { break Ok::<_, anyhow::Error>(value); }
                if value.get("id").is_some() && value.get("method").is_some() {
                    write_message(&mut input, &json!({"jsonrpc":"2.0","id":value["id"],"error":{"code":-32601,"message":"client method unsupported"}})).await?;
                }
            }
        }).await.context("language server initialize timed out")??;
        ensure!(init.get("error").is_none(), "language server rejected initialize");
        let capabilities = &init["result"]["capabilities"];
        ensure!(capabilities["positionEncoding"].as_str().is_none_or(|e| e == "utf-16"), "language server requires unsupported position encoding");
        let legend: Vec<String> = capabilities["semanticTokensProvider"]["legend"]["tokenTypes"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        let schema_supported = capabilities["experimental"]["kustoSchemaVersion"] == 1;
        write_message(&mut input, &json!({"jsonrpc":"2.0","method":"initialized","params":{}})).await?;
        let _ = events.send(Event::Ready).await;
        let (messages_tx, mut messages) = mpsc::channel(32);
        // A dedicated reader avoids losing a partially read frame when another select branch wins.
        let reader = tokio::spawn(async move {
            loop {
                let message = read_message(&mut output).await;
                let failed = message.is_err();
                if messages_tx.send(message).await.is_err() || failed { break; }
            }
        });
        let mut next_id = 2_u64;
        let mut pending: HashMap<u64, (Request, i64, tokio::time::Instant)> = HashMap::new();
        let mut opened = false;
        let mut current_version = -1;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let serving = async {
            loop {
                tokio::select! {
                    action = actions.recv() => {
                        let Some(action) = action else { break; };
                        let request = match action {
                            Action::Shutdown => break,
                            Action::Document { version, text } => {
                                current_version = version;
                                let value = if !opened { opened = true; json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":URI,"languageId":"kusto","version":version,"text":text}}}) }
                                    else { json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":URI,"version":version},"contentChanges":[{"text":text}]}}) };
                                write_message(&mut input, &value).await?;
                                Some((Request::Tokens, version, "textDocument/semanticTokens/full", json!({"textDocument":{"uri":URI}})))
                            }
                            Action::Schema(schema) => {
                                if schema_supported {
                                    write_message(&mut input, &json!({"jsonrpc":"2.0","method":"kusto/setSchema","params":{"uri":URI,"schema":schema}})).await?;
                                    opened.then_some((Request::Tokens, current_version, "textDocument/semanticTokens/full", json!({"textDocument":{"uri":URI}})))
                                } else {
                                    let _ = events.send(Event::Status("language server does not advertise kusto schema v1".into())).await;
                                    None
                                }
                            }
                            Action::Completion { version, line, character } => Some((Request::Completion, version, "textDocument/completion", json!({"textDocument":{"uri":URI},"position":{"line":line,"character":character}}))),
                            Action::Hover { version, line, character } => Some((Request::Hover, version, "textDocument/hover", json!({"textDocument":{"uri":URI},"position":{"line":line,"character":character}}))),
                        };
                        if let Some((kind, version, method, params)) = request {
                            // Cancel obsolete semantic work without ever applying an old version.
                            let obsolete: Vec<_> = pending.iter().filter(|(_, (k, v, _))| matches!(k, Request::Tokens) && *v <= version).map(|(id, _)| *id).collect();
                            for id in obsolete {
                                write_message(&mut input, &json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}})).await?;
                                pending.remove(&id);
                            }
                            ensure!(pending.len() < 64, "language server request queue is full");
                            let id = next_id; next_id += 1;
                            pending.insert(id, (kind, version, tokio::time::Instant::now()));
                            write_message(&mut input, &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
                        }
                    }
                    value = messages.recv() => {
                        let value = value.context("language server reader stopped; pending requests failed")??;
                        if value["method"] == "textDocument/publishDiagnostics" {
                            let p = &value["params"];
                            if p["uri"] == URI && p["version"].as_i64() == Some(current_version) {
                                let _ = events.send(Event::Diagnostics { version:current_version, diagnostics:p["diagnostics"].clone() }).await;
                            }
                        } else if value.get("method").is_some() && value.get("id").is_some() {
                            write_message(&mut input, &json!({"jsonrpc":"2.0","id":value["id"],"error":{"code":-32601,"message":"client method unsupported"}})).await?;
                        } else if let Some(id) = value["id"].as_u64() && let Some((kind, version, _)) = pending.remove(&id) {
                            if version != current_version { continue; }
                            if let Some(error) = value.get("error") {
                                if error["code"] != -32801 && error["code"] != -32800 {
                                    let _ = events.send(Event::Status(format!("language server request failed: {}", crate::safe_text(&error.to_string())))).await;
                                }
                                continue;
                            }
                            let event = match kind {
                                Request::Tokens => {
                                    let data = serde_json::from_value(value["result"]["data"].clone()).context("invalid semantic token data")?;
                                    Event::Tokens { version, data, legend:legend.clone() }
                                },
                                Request::Completion => Event::Completion { version, value:value["result"].clone() },
                                Request::Hover => Event::Hover { version, value:value["result"].clone() },
                            };
                            let _ = events.send(event).await;
                        }
                    }
                    _ = tick.tick() => {
                        if pending.values().any(|(_, _, since)| since.elapsed() > Duration::from_secs(20)) { bail!("language server request timed out; pending requests failed"); }
                    }
                }
            }
            if opened { write_message(&mut input, &json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":URI}}})).await?; }
            let id = next_id;
            write_message(&mut input, &json!({"jsonrpc":"2.0","id":id,"method":"shutdown","params":null})).await?;
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                while let Some(Ok(message)) = messages.recv().await { if message["id"] == id { break; } }
            }).await;
            write_message(&mut input, &json!({"jsonrpc":"2.0","method":"exit","params":null})).await?;
            Ok::<_, anyhow::Error>(())
        }.await;
        reader.abort();
        serving
    }.await;
    stderr_task.abort();
    drop(input);
    if tokio::time::timeout(Duration::from_secs(1), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    result
}

#[derive(Clone, Debug)]
pub struct TokenSpan {
    pub line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: String,
}
pub fn decode_tokens(text: &[String], data: &[u32], legend: &[String]) -> Result<Vec<TokenSpan>> {
    ensure!(
        data.len().is_multiple_of(5),
        "semantic token tuple length is invalid"
    );
    let (mut line, mut col) = (0_usize, 0_usize);
    let mut spans = Vec::new();
    for token in data.as_chunks::<5>().0 {
        line = line
            .checked_add(token[0] as usize)
            .context("semantic token line overflow")?;
        col = if token[0] == 0 {
            col.checked_add(token[1] as usize)
                .context("semantic token column overflow")?
        } else {
            token[1] as usize
        };
        let source = text
            .get(line)
            .context("semantic token line out of bounds")?;
        let start = utf16_to_byte(source, col)
            .context("semantic token starts inside surrogate/outside line")?;
        let end = utf16_to_byte(source, col + token[2] as usize)
            .context("semantic token ends inside surrogate/outside line")?;
        let kind = legend
            .get(token[3] as usize)
            .context("semantic token type outside legend")?
            .clone();
        spans.push(TokenSpan {
            line,
            start_byte: start,
            end_byte: end,
            kind,
        });
    }
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_roundtrip_display_and_tokens() {
        let s = "a😀界e\u{301}";
        assert_eq!(utf16_to_byte(s, 3), Some(5));
        assert_eq!(utf16_to_byte(s, 2), None);
        assert_eq!(char_to_utf16(s, 2), 3);
        assert_eq!(byte_to_display(s, 8), Some(5));
        let tokens = decode_tokens(&[s.into()], &[0, 1, 2, 0, 0], &["string".into()]).unwrap();
        assert_eq!(&s[tokens[0].start_byte..tokens[0].end_byte], "😀");
        assert!(decode_tokens(&[s.into()], &[0, 2, 1, 0, 0], &["string".into()]).is_err());
    }
    #[tokio::test]
    async fn transport_unicode_frames_and_eof() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let data = json!({"text":"😀界","id":7});
        write_message(&mut writer, &data).await.unwrap();
        drop(writer);
        let mut reader = BufReader::new(reader);
        assert_eq!(read_message(&mut reader).await.unwrap(), data);
        assert!(read_message(&mut reader).await.is_err());
        let mut bad = BufReader::new(&b"Content-Length: 999999999\r\n\r\n"[..]);
        assert!(read_message(&mut bad).await.is_err());
    }
    #[tokio::test]
    #[ignore = "requires DATAEXPLORER_TEST_LSP pointing to the actual kusto-lsp executable"]
    async fn external_server_interop() {
        let command =
            std::env::var("DATAEXPLORER_TEST_LSP").expect("actual kusto-lsp executable required");
        let args = std::env::var("DATAEXPLORER_TEST_LSP_DLL")
            .map(|dll| vec![dll, "--stdio".into()])
            .unwrap_or_else(|_| vec!["--stdio".into()]);
        let mut server = Handle::start(LanguageServer { command, args });
        let ready = tokio::time::timeout(Duration::from_secs(25), server.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ready, Event::Ready), "{ready:?}");
        server.document(1, "print message='😀界'".into()).unwrap();
        server.document(2, "Events | project Name".into()).unwrap();
        server.schema(json!({"version":1,"cluster":"https://test.example","database":"Test","tables":[{"name":"Events","columns":[{"name":"Name","type":"string"}]}],"functions":[]})).unwrap();
        let (mut tokens, mut diagnostics) = (false, false);
        tokio::time::timeout(Duration::from_secs(20), async {
            while !(tokens && diagnostics) {
                match server.events.recv().await.unwrap() {
                    Event::Tokens {
                        version: 2,
                        data,
                        legend,
                    } => {
                        assert!(
                            !decode_tokens(&["Events | project Name".into()], &data, &legend)
                                .unwrap()
                                .is_empty()
                        );
                        tokens = true;
                    }
                    Event::Diagnostics {
                        version: 2,
                        diagnostics: d,
                    } => {
                        if d.as_array().is_some_and(Vec::is_empty) {
                            diagnostics = true;
                        }
                    }
                    Event::Status(message) => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        server.completion(2, 0, 20).unwrap();
        server.hover(2, 0, 19).unwrap();
        let (mut completed, mut hovered) = (false, false);
        tokio::time::timeout(Duration::from_secs(20), async {
            while !(completed && hovered) {
                match server.events.recv().await.unwrap() {
                    Event::Completion { version: 2, value } => {
                        let items = value
                            .as_array()
                            .or_else(|| value["items"].as_array())
                            .expect("completion shape");
                        assert!(items.iter().any(|v| v["label"] == "Name"));
                        completed = true;
                    }
                    Event::Hover { version: 2, value } => {
                        assert!(!value.is_null());
                        hovered = true;
                    }
                    Event::Status(message) => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        server
            .document(3, "Events | project Missing".into())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match server.events.recv().await.unwrap() {
                    Event::Diagnostics {
                        version: 3,
                        diagnostics,
                    } if diagnostics.as_array().is_some_and(|a| !a.is_empty()) => break,
                    Event::Status(message) => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        server.schema(Value::Null).unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match server.events.recv().await.unwrap() {
                    Event::Diagnostics {
                        version: 3,
                        diagnostics,
                    } if diagnostics.as_array().is_some_and(Vec::is_empty) => break,
                    Event::Status(message) => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let documentation = crate::query_library::Documentation {
            description: "Unicode description \u{1f600}".into(),
            parameters: vec![
                crate::query_library::Parameter {
                    name: "name".into(),
                    kind: "string".into(),
                    description: "Person".into(),
                    default: Some("O'Brien \"quoted\"".into()),
                },
                crate::query_library::Parameter {
                    name: "limit".into(),
                    kind: "long".into(),
                    description: "Maximum rows".into(),
                    default: Some("10".into()),
                },
            ],
        };
        let query = crate::query_library::document(&documentation, "print name, limit").unwrap();
        server.document(4, query).unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match server.events.recv().await.unwrap() {
                    Event::Diagnostics {
                        version: 4,
                        diagnostics,
                    } => {
                        assert!(
                            diagnostics.as_array().is_some_and(Vec::is_empty),
                            "{diagnostics}"
                        );
                        break;
                    }
                    Event::Status(message) => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        server.shutdown().await;
    }
}
