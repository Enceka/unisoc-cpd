//! W7: the web face of the resident owner.
//!
//! The web server is a **socket client**, never a channel owner: every button
//! and every poll becomes the same line-JSON request the CLI makes, and the
//! single-threaded serve loop stays the only thing that touches the CP.  That
//! is why this capability refuses to run without a socket path.

use super::{flag_value, positionals, Capability, Outcome};
use crate::context::Context;
use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Web;

impl Capability for Web {
    fn name(&self) -> &'static str {
        "web"
    }

    fn summary(&self) -> &'static str {
        "W7 web UI: a browser face over a running serve (web [ADDR:PORT] --socket PATH; runs until killed)"
    }

    fn native_only(&self) -> bool {
        true
    }

    fn run(&self, _ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let Some(socket) = flag_value(args, "--socket").map(PathBuf::from) else {
            bail!("web needs --socket: it drives a running serve, never the channels directly");
        };
        let listen = positionals(args)
            .first()
            .cloned()
            .unwrap_or_else(|| "127.0.0.1:8080".into());
        let listener =
            TcpListener::bind(&listen).with_context(|| format!("cannot listen on {listen}"))?;
        println!("unisoc-cpd web: http://{listen} (daemon on {})", socket.display());
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let socket = socket.clone();
            std::thread::spawn(move || {
                let _ = serve_conn(stream, &socket);
            });
        }
        unreachable!("the listener never ends");
    }
}

// ----------------------------------------------------------------- requests

fn serve_conn(mut stream: TcpStream, socket: &Path) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = String::new();
    if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf)?;
        body = String::from_utf8_lossy(&buf).into_owned();
    }
    let (path, _query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    route(&mut stream, &method, &path, &body, socket);
    Ok(())
}

fn ask(socket: &Path, request: &Value) -> Result<Value> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    let mut writer = stream.try_clone()?;
    writer.write_all(request.to_string().as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}

fn run_cap(socket: &Path, capability: &str, args: &[&str]) -> Value {
    let request = json!({ "action": "run", "capability": capability, "args": args });
    ask(socket, &request).unwrap_or_else(|e| json!({ "error": format!("{e}") }))
}

fn route(stream: &mut TcpStream, method: &str, path: &str, body: &str, socket: &Path) {
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(stream, "200 OK", "text/html; charset=utf-8", PAGE.to_string());
        }
        ("GET", "/api/state") => pass(stream, socket, &json!({ "action": "state" })),
        ("GET", "/api/urc") => pass(stream, socket, &json!({ "action": "urc", "limit": 40 })),
        ("GET", "/api/messages") => {
            pass(stream, socket, &json!({ "action": "messages", "limit": 50 }))
        }
        ("GET", "/api/status") => {
            let answer = json!({
                "register": run_cap(socket, "register", &["status"]),
                "signal": run_cap(socket, "signal", &[]),
                "ims": run_cap(socket, "ims", &["status"]),
            });
            respond_json(stream, &answer);
        }
        ("POST", "/api/send") => {
            let to = form_value(body, "to").unwrap_or_default();
            let text = form_value(body, "text").unwrap_or_default();
            respond_json(stream, &run_cap(socket, "sms", &["send", &to, &text]));
        }
        ("POST", "/api/dial") => {
            let number = form_value(body, "number").unwrap_or_default();
            respond_json(stream, &run_cap(socket, "call", &["dial", &number]));
        }
        ("POST", "/api/answer") => {
            respond_json(stream, &run_cap(socket, "call", &["answer"]));
        }
        ("POST", "/api/hangup") => {
            respond_json(stream, &run_cap(socket, "call", &["hangup"]));
        }
        _ => respond(stream, "404 Not Found", "text/plain; charset=utf-8", "not found\n".into()),
    }
}

fn pass(stream: &mut TcpStream, socket: &Path, request: &Value) {
    let answer = ask(socket, request).unwrap_or_else(|e| json!({ "error": format!("{e}") }));
    respond_json(stream, &answer);
}

fn respond_json(stream: &mut TcpStream, value: &Value) {
    respond(stream, "200 OK", "application/json", value.to_string());
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: String) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) =
                ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
            {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        if b[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----------------------------------------------------------------- the page

const PAGE: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>unisoc-cpd</title>
<style>
 body{font-family:system-ui,sans-serif;background:#111;color:#ddd;margin:0;padding:12px}
 h1{font-size:18px} h2{font-size:15px;margin:14px 0 6px}
 .chips span{display:inline-block;background:#1d2b1d;border:1px solid #2e4d2e;border-radius:10px;padding:2px 10px;margin:2px;font-size:12px}
 pre{background:#181818;border:1px solid #333;border-radius:6px;padding:8px;min-height:14px;max-height:220px;overflow:auto;font-size:12px;white-space:pre-wrap}
 input,button{font-size:14px;border-radius:6px;border:1px solid #444;background:#222;color:#eee;padding:6px 10px;margin:2px}
 button{cursor:pointer;background:#28422a} button.red{background:#5a2323}
 #banner{display:none;position:fixed;inset:0;background:rgba(120,20,20,.94);z-index:9;text-align:center;padding-top:30vh}
 #banner button{font-size:22px;margin:12px}
 .msg{border-bottom:1px solid #2a2a2a;padding:6px 2px;font-size:13px}
 .from{color:#8bc78b}
</style></head><body>
<h1>unisoc-cpd <span id="chips" class="chips"></span></h1>
<div id="banner"><div id="bnr-txt" style="font-size:24px">+CRING: VOICE</div>
 <button onclick="act('answer')">接听</button><button class="red" onclick="act('hangup')">挂断</button></div>

<h2>短信 · inbox</h2><div id="msgs">…</div>
<h2>发短信</h2>
<div><input id="to" placeholder="+86…" size="14"> <input id="text" placeholder="内容" size="24">
<button onclick="sendSms()">发送</button></div><pre id="sms-out"></pre>

<h2>电话</h2>
<div><input id="num" placeholder="号码" size="14">
<button onclick="dial()">呼叫</button>
<button onclick="act('answer')">接听</button>
<button class="red" onclick="act('hangup')">挂断</button></div><pre id="call-out"></pre>

<h2>状态（register / signal / ims）</h2>
<div><button onclick="status()">刷新状态</button></div><pre id="stat-out"></pre>

<h2>事件流（urc）</h2><pre id="urc-out">…</pre>

<script>
function out(id, resp){ document.getElementById(id).textContent =
  (resp.output||[]).join('\n') + (resp.status ? ('\n['+resp.status+']') : '')
  + (resp.error ? ('\n[error] '+resp.error) : ''); }
async function get(u){ const r = await fetch(u); return r.json(); }
async function post(u, data){ const r = await fetch(u, {method:'POST',
  headers:{'Content-Type':'application/x-www-form-urlencoded'},
  body: new URLSearchParams(data).toString()}); return r.json(); }
async function refreshState(){ try{ const d = await get('/api/state'); const s = d.state||{};
  const a = s.at||{}; const c = (s.channels||{}).cmd||{};
  document.getElementById('chips').innerHTML =
    '<span>AT ok '+ (a.ok||0) +'/'+ (a.commands||0) +'</span>' +
    '<span>timeout ' + (a.timeouts||0) + '</span>' +
    '<span>opens ' + (c.opens||0) + '</span>' +
    '<span>last_ok ' + (s.last_ok_age_s==null ? 'never' : Math.round(s.last_ok_age_s)+'s') + '</span>';
 }catch(e){} }
async function refreshUrc(){ try{ const d = await get('/api/urc'); const us = d.urcs||[];
  document.getElementById('urc-out').textContent = us.map(function(u){return u.at+'  '+u.urc;}).join('\n');
  const ring = us.filter(function(u){return u.urc.indexOf('+CRING')===0;});
  if(ring.length){ const b = document.getElementById('banner');
    if(b.style.display!=='block'){ b.style.display='block';
      document.getElementById('bnr-txt').textContent = ring[ring.length-1].urc; } }
  else { document.getElementById('banner').style.display='none'; }
 }catch(e){} }
async function refreshMsgs(){ try{ const d = await get('/api/messages'); const ms = d.messages||[];
  document.getElementById('msgs').innerHTML = ms.map(function(m){
   return '<div class="msg"><span class="from">'+m.from+'</span> '
    +(m.timestamp||'')+'<br>'+ (m.text||'').replace(/&/g,'&amp;').replace(/</g,'&lt;') +'</div>';}).join('')
   || '（空）';
 }catch(e){} }
async function sendSms(){ const r = await post('/api/send',
  {to:document.getElementById('to').value, text:document.getElementById('text').value});
 out('sms-out', r); refreshMsgs(); }
async function dial(){ const r = await post('/api/dial', {number:document.getElementById('num').value});
 out('call-out', r); }
async function act(w){ const r = await post('/api/'+w, {}); out('call-out', r);
 if(w!=='hangup') setTimeout(function(){document.getElementById('banner').style.display='none';}, 800); }
async function status(){ const d = await get('/api/status');
 document.getElementById('stat-out').textContent =
  '-- register --\n' + (d.register.output||[]).join('\n') +
  '\n-- signal --\n' + (d.signal.output||[]).join('\n') +
  '\n-- ims --\n' + (d.ims.output||[]).join('\n'); }
setInterval(refreshState, 5000); setInterval(refreshUrc, 2000); setInterval(refreshMsgs, 8000);
refreshState(); refreshUrc(); refreshMsgs();
</script></body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_reads_utf8_and_plus() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn form_value_picks_named_fields() {
        assert_eq!(form_value("to=123&text=hello", "text"), Some("hello".into()));
        assert_eq!(form_value("to=123", "text"), None);
    }
}
