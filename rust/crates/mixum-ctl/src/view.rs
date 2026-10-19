//! 本地窗口视图：单文件 HTML 与 JPEG 帧录制。

use crate::transport::{Client, Error};
use futures_util::SinkExt;
use mixum_protocol::frames::unmarshal_video;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

const HTML: &str = r#"<!doctype html><meta charset=utf-8><title>mixum 视图</title>
<style>body{font:14px system-ui;margin:2rem;background:#111;color:#eee}button{margin:.25rem;padding:.4rem}canvas{max-width:100%;background:#222}</style>
<h1>mixum 窗口</h1><div id=list>正在连接…</div><canvas id=c></canvas><pre id=meta></pre>
<script>
const list=document.querySelector('#list'), c=document.querySelector('#c'), x=c.getContext('2d'), meta=document.querySelector('#meta');
let socket=null;
async function load(){const r=await fetch('/api/windows',{cache:'no-store'});const data=await r.json();list.textContent='';data.windows.forEach(w=>{const b=document.createElement('button');b.textContent=`${w.id}: ${w.title||w.appId}`;b.onclick=()=>watch(w.id);list.append(b)})}
function watch(id){
  if(socket){try{socket.close()}catch(e){}}
  const s=new WebSocket(`ws://${location.host}/api/stream/${id}`);socket=s;s.binaryType='arraybuffer';
  let pending=false, latest=null;
  s.onmessage=e=>{
    const b=new Uint8Array(e.data); if(b.length<27||b[0]!==1)return;
    const dv=new DataView(e.data);
    latest={jpeg:b.slice(27,27+dv.getUint32(13,true)),w:dv.getUint32(5,true),h:dv.getUint32(9,true),
            cx:dv.getInt16(19,true),cy:dv.getInt16(21,true),cw:dv.getUint16(23,true),ch:dv.getUint16(25,true),
            cursor:(dv.getUint16(17,true)&2)!==0};
    if(!pending) pump();
  };
  // 只保留最新帧、串行解码:避免旧帧晚到覆盖新帧,也避免每帧重设 canvas 尺寸清屏。
  function pump(){
    if(!latest){pending=false;return}
    pending=true; const f=latest; latest=null;
    createImageBitmap(new Blob([f.jpeg],{type:'image/jpeg'})).then(img=>{
      if(c.width!==f.w||c.height!==f.h){c.width=f.w;c.height=f.h}
      x.drawImage(img,0,0); img.close();
      if(f.cursor){x.strokeStyle='#f44';x.strokeRect(f.cx,f.cy,f.cw,f.ch)}
      meta.textContent=`窗口 ${id} ${f.w}x${f.h} 光标 ${f.cx},${f.cy}`;
      pending=false; pump();
    }).catch(()=>{pending=false; pump()});
  }
}
load();
</script>"#;

pub async fn serve(client: Client, bind: &str) -> Result<(), Error> {
    let listener = TcpListener::bind(bind).await?;
    println!("本地视图: http://{bind}");
    loop {
        let (mut stream, _) = listener.accept().await?;
        let client = client.clone();
        tokio::spawn(async move {
            let mut probe = [0u8; 1024];
            let peeked = stream.peek(&mut probe).await.unwrap_or(0);
            let request_head = String::from_utf8_lossy(&probe[..peeked]);
            if request_head.contains("Upgrade: websocket")
                || request_head.contains("upgrade: websocket")
            {
                if let Ok(mut socket) = accept_async(stream).await {
                    let window = request_head
                        .split_whitespace()
                        .nth(1)
                        .and_then(|path| path.rsplit('/').next())
                        .and_then(|id| id.parse::<u32>().ok());
                    if let Some(window) = window {
                        let _ = client
                            .call(
                                "display.attach",
                                serde_json::json!({ "window": window, "fps": 10 }),
                            )
                            .await;
                        let mut frames = client.subscribe_frames();
                        while let Ok(frame) = frames.recv().await {
                            let matches = unmarshal_video(&frame.0)
                                .map(|video| video.window_id == window)
                                .unwrap_or(false);
                            if matches
                                && socket.send(Message::Binary(frame.0.into())).await.is_err()
                            {
                                break;
                            }
                        }
                        let _ = client
                            .call("display.detach", serde_json::json!({ "window": window }))
                            .await;
                    }
                }
                return;
            }
            let mut request = [0u8; 4096];
            let n = match stream.read(&mut request).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let line = String::from_utf8_lossy(&request[..n]);
            let path = line.split_whitespace().nth(1).unwrap_or("/");
            if path == "/api/windows" {
                let body = match client.call("display.list", serde_json::json!({})).await {
                    Ok(e) => e
                        .payload
                        .unwrap_or_else(|| serde_json::json!({"windows": []}))
                        .to_string(),
                    Err(_) => r#"{"windows":[]}"#.to_owned(),
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
                let _ = stream.write_all(response.as_bytes()).await;
            } else {
                let body = HTML.as_bytes();
                let header = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(body).await;
            }
        });
    }
}

pub async fn record(
    client: &Client,
    window: u32,
    out: &Path,
    count: usize,
) -> Result<usize, Error> {
    client
        .call(
            "display.attach",
            serde_json::json!({"window": window, "fps": 10}),
        )
        .await?;
    std::fs::create_dir_all(out)?;
    let mut saved = 0;
    let mut frames = client.subscribe_frames();
    while saved < count {
        let frame = frames
            .recv()
            .await
            .map_err(|e| Error::WebSocket(format!("帧流已关闭: {e}")))?;
        let parsed = unmarshal_video(&frame.0).map_err(|e| Error::Protocol(e.to_string()))?;
        if parsed.window_id != window {
            continue;
        }
        let path: PathBuf = out.join(format!("frame-{saved:04}.jpg"));
        std::fs::write(path, parsed.data)?;
        saved += 1;
    }
    let _ = client
        .call("display.detach", serde_json::json!({"window": window}))
        .await;
    Ok(saved)
}
