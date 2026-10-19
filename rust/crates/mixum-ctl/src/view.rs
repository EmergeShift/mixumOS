//! 本地窗口视图：单文件 HTML 与 JPEG 帧录制。

use crate::transport::{Client, Error};
use futures_util::SinkExt;
use mixum_protocol::frames::unmarshal_video;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

const HTML: &str = r#"<!doctype html><meta charset=utf-8><title>mixum 视图</title>
<style>
body{font:14px system-ui;margin:2rem;background:#111;color:#eee}
button{margin:.25rem;padding:.4rem}
.stage{position:relative;display:inline-block;background:#222;line-height:0}
canvas{display:block;max-width:100%;background:#222}
#o{position:absolute;left:0;top:0;pointer-events:none}
#meta{color:#8b949e}
</style>
<h1>mixum 窗口</h1><div id=list>正在连接…</div>
<div class="stage"><canvas id=c></canvas><canvas id=o></canvas></div><pre id=meta></pre>
<script>
const list=document.querySelector('#list'), c=document.querySelector('#c'), x=c.getContext('2d');
const o=document.querySelector('#o'), ox=o.getContext('2d'), meta=document.querySelector('#meta');
let socket=null;
async function load(){const r=await fetch('/api/windows',{cache:'no-store'});const data=await r.json();list.textContent='';data.windows.forEach(w=>{const b=document.createElement('button');b.textContent=`${w.id}: ${w.title||w.appId}`;b.onclick=()=>watch(w.id);list.append(b)})}
function watch(id){
  if(socket){try{socket.close()}catch(e){}}
  const s=new WebSocket(`ws://${location.host}/api/stream/${id}`);socket=s;s.binaryType='arraybuffer';
  let chain=Promise.resolve(), haveFull=false;
  s.onmessage=e=>{
    const b=new Uint8Array(e.data); if(b.length<27||b[0]!==1)return;
    const dv=new DataView(e.data);
    const flags=dv.getUint16(17,true), size=dv.getUint32(13,true);
    const fullW=dv.getUint32(5,true), fullH=dv.getUint32(9,true);
    const patch=(flags&4)!==0;
    const cursor=(flags&2)!==0;
    const cx=dv.getInt16(19,true), cy=dv.getInt16(21,true), cw=dv.getUint16(23,true), ch=dv.getUint16(25,true);
    let x0=0,y0=0,w=fullW,h=fullH,start=27,format=0;
    if(patch){
      if(size<20)return;
      x0=dv.getUint32(27,true); y0=dv.getUint32(31,true);
      w=dv.getUint32(35,true); h=dv.getUint32(39,true); format=dv.getUint32(43,true);
      start=47;
    }
    const bytes=b.slice(start,27+size);
    chain=chain.then(()=>draw({id,fullW,fullH,x:x0,y:y0,w,h,format,cursor,cx,cy,cw,ch,bytes,patch}))
      .catch(()=>{});
  };
  async function draw(f){
    if(!f.patch){
      const mime=f.bytes[0]===0x89?'image/png':'image/jpeg';
      const img=await createImageBitmap(new Blob([f.bytes],{type:mime}));
      if(c.width!==f.fullW||c.height!==f.fullH){c.width=f.fullW;c.height=f.fullH;o.width=f.fullW;o.height=f.fullH;}
      x.clearRect(0,0,c.width,c.height); x.drawImage(img,0,0); img.close(); haveFull=true;
    } else if(haveFull && f.w>0 && f.h>0 && f.bytes.length>0){
      const mime=f.format===2?'image/png':'image/jpeg';
      const img=await createImageBitmap(new Blob([f.bytes],{type:mime}));
      x.drawImage(img,f.x,f.y,f.w,f.h); img.close();
    }
    ox.clearRect(0,0,o.width,o.height);
    if(f.cursor){ox.strokeStyle='#f44';ox.lineWidth=1;ox.strokeRect(f.cx,f.cy,f.cw,f.ch);}
    meta.textContent=`窗口 ${f.id} ${f.fullW}x${f.fullH}`+(f.patch?` · patch ${f.w}x${f.h}@${f.x},${f.y}`:' · full');
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
                                serde_json::json!({ "window": window, "fps": 10, "codec": "png" }),
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
