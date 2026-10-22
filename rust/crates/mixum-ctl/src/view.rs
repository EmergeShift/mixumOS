//! 本地窗口视图:管理页 + 每容器窗口独立视图(单文件 HTML 与 JPEG 帧录制)。

use crate::transport::{Client, Error};
use futures_util::{SinkExt, StreamExt};
use mixum_protocol::frames::unmarshal_video;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

/// 首帧到达前窗口尺寸未知,先给个默认值,JS 端首帧后 resize。
const INDEX_HTML: &str = r#"<!doctype html><meta charset=utf-8><title>mixum 窗口</title>
<style>body{font:14px system-ui;margin:2rem;background:#111;color:#eee}button{margin:.25rem;padding:.4rem}.row{display:flex;gap:.5rem;align-items:center;margin:.3rem 0}.st{color:#8a8;font-size:12px}</style>
<h1>mixum 窗口</h1><div id=list>正在连接…</div>
<script>
const list=document.querySelector('#list');
async function load(){
  let ws=[];
  try{ws=(await(await fetch('/api/windows')).json()).windows||[];}catch(e){}
  list.textContent='';
  if(!ws.length){list.innerHTML='<i>暂无窗口(容器内 GUI 应用未运行或未连接 mixcomp)</i>';return;}
  ws.forEach(w=>{
    const row=document.createElement('div');row.className='row';
    const b=document.createElement('button');b.textContent=`打开 ${w.id}: ${w.title||w.appId||''}`;
    b.onclick=()=>window.open('/view/'+w.id,'mixum-'+w.id,'popup=yes,width=480,height=320');
    const st=document.createElement('span');st.className='st';st.textContent=w.mode?`模式 ${w.mode}`:'';
    row.append(b,st);list.append(row);
  });
}
load();setInterval(load,2000);
</script>"#;

const VIEW_HTML: &str = r#"<!doctype html><meta charset=utf-8><title>mixum 视图</title>
<style>body{font:13px system-ui;margin:0;background:#111;color:#eee;display:flex;flex-direction:column;height:100vh}#bar{padding:.3rem .6rem;background:#1a1a1a;border-bottom:1px solid #333;white-space:nowrap;overflow:hidden;display:flex;gap:.5rem;align-items:center}#title{flex:1;overflow:hidden;text-overflow:ellipsis}.badge{font-size:11px;padding:.1rem .4rem;border-radius:3px;background:#444}.badge.user{background:#2a6e2a}.badge.agent{background:#7a2a2a}#tk{padding:.2rem .6rem}#au{padding:.2rem .6rem}#vol{width:90px}#off{width:70px;accent-color:#e0b050}#stage{position:relative;display:inline-block;align-self:center;max-width:100%;background:#222;line-height:0;overflow:auto}#c{display:block;max-width:100%;background:#222;outline:none}#o{position:absolute;left:0;top:0;pointer-events:none}#i{position:absolute;width:1px;height:1px;opacity:0;border:0;padding:0;resize:none;overflow:hidden;pointer-events:none}</style>
<div id=bar><span id=title>窗口 __ID__ 连接中…</span><span id=mode class=badge>free</span><button id=tk>接管</button><button id=au>音频</button><input id=vol type=range min=0 max=100 value=80 title=音量><input id=off type=range min=-200 max=200 step=10 value=0 title=A/V偏移(ms)><span id=af class=badge></span></div>
<div id=stage><canvas id=c tabindex=0></canvas><canvas id=o></canvas><textarea id=i aria-label="输入法输入" autocomplete=off autocorrect=off spellcheck=false></textarea></div>
<script>
const id=__ID__, c=document.querySelector('#c'), x=c.getContext('2d'), o=document.querySelector('#o'), ox=o.getContext('2d'), ime=document.querySelector('#i'),
      bar=document.querySelector('#bar'), modeEl=document.querySelector('#mode'), tk=document.querySelector('#tk'),
      au=document.querySelector('#au'), vol=document.querySelector('#vol'), off=document.querySelector('#off'), af=document.querySelector('#af');
let mode='free', stream=null, haveFull=false, audio=null;
const pending=new Map();
function title(t){document.title=`${t} — mixum`;document.querySelector('#title').textContent=`${id}: ${t}`;}
function setMode(m){mode=m;modeEl.textContent=m;modeEl.className='badge '+m;tk.textContent=m==='user'?'释放':'接管';}
function ctl(op,payload){
  const rid=crypto.randomUUID();
  return new Promise((res,rej)=>{
    const t=setTimeout(()=>{pending.delete(rid);rej(new Error('超时: '+op));},10000);
    pending.set(rid,e=>{clearTimeout(t);e.op&&e.op.endsWith('.err')?rej(new Error(e.payload?.message||op+' 失败')):res(e.payload);});
    stream.send(JSON.stringify({v:1,id:rid,op,payload}));
  });
}
function stopAudio(sendDetach){
  if(audio){const a=audio;audio=null;try{a.ctx.close();}catch(e){}}
  au.textContent='音频';af.textContent='';
  if(sendDetach)ctl('audio.detach',{}).catch(()=>{});
}
au.onclick=async()=>{
  if(audio){stopAudio(true);return;}
  try{
    const fmt=await ctl('audio.attach',{});
    const ctx=new AudioContext({sampleRate:fmt.sampleRate});
    await ctx.resume();
    const gain=ctx.createGain();gain.gain.value=vol.value/100;gain.connect(ctx.destination);
    audio={ctx,gain,next:0,lead:0.06+off.value/1000,frames:0,lastSeq:-1,lastPts:-1,drops:0,sampleRate:fmt.sampleRate,channels:fmt.channels};
    au.textContent='静音';
  }catch(e){bar.textContent=`${id}: 音频 ${e.message}`;}
};
vol.oninput=()=>{if(audio)audio.gain.gain.value=vol.value/100;};
off.oninput=()=>{if(audio)audio.lead=Math.max(0.01,0.06+off.value/1000);};
function playAudio(buf){
  const a=audio;if(!a)return;
  const b=new Uint8Array(buf);if(b.length<15)return;
  const dv=new DataView(buf,1,12);
  const seq=dv.getUint32(0,true), pts=Number(dv.getBigUint64(4,true));
  const bytes=b.subarray(13);const pcm=new Int16Array(bytes.length>>1);new Uint8Array(pcm.buffer).set(bytes);
  const frames=pcm.length/a.channels;
  if(a.lastSeq>=0&&seq!==a.lastSeq+1)a.drops++;
  a.lastSeq=seq;
  const ab=a.ctx.createBuffer(a.channels,frames,a.sampleRate);
  for(let ch=0;ch<a.channels;ch++){const out=ab.getChannelData(ch);for(let i=0;i<frames;i++)out[i]=pcm[i*a.channels+ch]/32768;}
  const src=a.ctx.createBufferSource();src.buffer=ab;src.connect(a.gain);
  const now=a.ctx.currentTime;
  const delta=a.lastPts>=0?(pts-a.lastPts)/1e6:0;
  a.lastPts=pts;
  let start=a.next;
  if(start<now+0.01||start>now+0.5)start=now+a.lead;
  src.start(start);
  a.next=start+(delta>0&&delta<=0.5?delta:ab.duration);a.frames++;
  if(a.frames%25===0)af.textContent=`${a.frames}${a.drops?` / 丢${a.drops}`:''}`;
}
tk.onclick=async()=>{
  try{
    if(mode==='user'){setMode((await ctl('display.takeover',{window:id,action:'release'})).mode??'free');return;}
    const force=mode==='agent'?confirm('agent 正持有输入,强制接管会让 agent 失去输入。继续?'):false;
    if(mode==='agent'&&!force)return;
    setMode((await ctl('display.takeover',{window:id,action:'claim',...(force?{force:true}:{})})).mode??'user');
    ime.focus({preventScroll:true});
  }catch(e){bar.textContent=`${id}: ${e.message}`;}
};
async function pollMode(){
  try{
    const d=await(await fetch('/api/windows')).json();
    const w=(d.windows||[]).find(w=>w.id===id);
    if(w)setMode(w.mode||'free');
  }catch(e){}
}
function toPhys(e){
  const r=c.getBoundingClientRect();
  return {x:Math.max(0,Math.min(c.width,(e.clientX-r.left)*(c.width/r.width))),
          y:Math.max(0,Math.min(c.height,(e.clientY-r.top)*(c.height/r.height)))};
}
const NAMED={Enter:'Return',Escape:'Escape',Backspace:'BackSpace',Tab:'Tab',Delete:'Delete',ArrowUp:'Up',ArrowDown:'Down',ArrowLeft:'Left',ArrowRight:'Right',Home:'Home',End:'End',PageUp:'Page_Up',PageDown:'Page_Down',Insert:'Insert',' ':'space'};
function keysym(e){if(e.key.length===1)return NAMED[e.key]??e.key;if(/^F\d{1,2}$/.test(e.key))return e.key;return NAMED[e.key]??null;}
function mods(e){return ['ctrl','alt','shift','logo'].filter((m,i)=>[e.ctrlKey,e.altKey,e.shiftKey,e.metaKey][i]);}
function sendInput(payload){if(mode==='user')ctl('display.input',{window:id,...payload}).catch(()=>{});}
let motion=null,motionRaf=false;
function flushMotion(){motionRaf=false;const p=motion;if(!p)return;motion=null;sendInput({type:'pointer_motion',x:p.x,y:p.y});}
c.addEventListener('pointermove',e=>{
  const p=toPhys(e);motion=p;
  if(!motionRaf){motionRaf=true;requestAnimationFrame(flushMotion);setTimeout(flushMotion,120);}
});
c.addEventListener('pointerdown',e=>{const r=c.getBoundingClientRect();ime.style.left=(e.clientX-r.left)+'px';ime.style.top=(e.clientY-r.top)+'px';ime.focus({preventScroll:true});sendInput({type:'pointer_button',button:e.button===2?'right':e.button===1?'middle':'left',action:'press'});e.preventDefault();});
c.addEventListener('pointerup',e=>sendInput({type:'pointer_button',button:e.button===2?'right':e.button===1?'middle':'left',action:'release'}));
c.addEventListener('wheel',e=>{sendInput({type:'pointer_axis',action:e.deltaY<0?'scroll_up':'scroll_down'});e.preventDefault();});
c.addEventListener('contextmenu',e=>e.preventDefault());
function sendText(t){if(!t||mode!=='user')return;ctl('display.input',{window:id,type:'text',text:t}).catch(()=>{});}
ime.addEventListener('compositionend',e=>{const t=e.data;ime.value='';sendText(t);});
ime.addEventListener('input',e=>{const t=ime.value;ime.value='';if(e.isComposing||!t)return;
  if(t.length===1&&t.charCodeAt(0)<0x7f)return;
  sendText(t);});
document.addEventListener('keydown',e=>{
  if(e.key==='F5'||e.key==='F11'||e.key==='F12')return;
  if(e.isComposing||e.key==='Process')return;
  const k=keysym(e);if(!k||mode!=='user')return;
  if(!e.repeat)sendInput({type:'key',keysym:k,action:'press',mods:mods(e)});
  e.preventDefault();
});
document.addEventListener('keyup',e=>{
  if(e.key==='F5'||e.key==='F11'||e.key==='F12')return;
  if(e.isComposing||e.key==='Process')return;
  const k=keysym(e);if(!k||mode!=='user')return;
  sendInput({type:'key',keysym:k,action:'release',mods:mods(e)});
  e.preventDefault();
});
function syncOverlay(){const r=c.getBoundingClientRect();o.style.width=r.width+'px';o.style.height=r.height+'px';}
addEventListener('resize',syncOverlay);
function setCanvas(w,h){
  const changed=c.width!==w||c.height!==h;
  if(changed){c.width=w;c.height=h;o.width=w;o.height=h;haveFull=false;syncOverlay();}
  return changed;
}
function drawCursor(f){
  ox.clearRect(0,0,o.width,o.height);
  if(f.cursor){ox.strokeStyle='#f44';ox.lineWidth=1;ox.strokeRect(f.cx,f.cy,f.cw,f.ch);}
}
async function draw(f){
  if(!f.patch){
    const mime=f.bytes[0]===0x89?'image/png':'image/jpeg';
    const img=await createImageBitmap(new Blob([f.bytes],{type:mime}));
    const changed=setCanvas(f.fullW,f.fullH);
    x.clearRect(0,0,c.width,c.height);
    x.drawImage(img,0,0);img.close();haveFull=true;
    if(changed&&!stream.sized&&f.fullW>0){stream.sized=true;try{window.resizeTo(f.fullW+16,f.fullH+56);}catch(e){}}
  }else if(haveFull&&f.w>0&&f.h>0&&f.bytes.length>0){
    const mime=f.format===2?'image/png':'image/jpeg';
    const img=await createImageBitmap(new Blob([f.bytes],{type:mime}));
    x.drawImage(img,f.x,f.y,f.w,f.h);img.close();
  }
  drawCursor(f);
}
function stream2(){
  const s=new WebSocket(`ws://${location.host}/api/stream/${id}`);
  s.binaryType='arraybuffer';
  let chain=Promise.resolve();
  s.onopen=()=>{stream=s;};
  s.onmessage=ev=>{
    if(typeof ev.data==='string'){
      try{const env=JSON.parse(ev.data);const cb=env.replyTo&&pending.get(env.replyTo);if(cb){pending.delete(env.replyTo);cb(env);}}catch(e){}
      return;
    }
    const b=new Uint8Array(ev.data);
    if(b[0]===2){playAudio(ev.data);return;}
    if(b[0]!==1)return;
    if(b.length<27)return;
    const dv=new DataView(ev.data);
    const flags=dv.getUint16(17,true), size=dv.getUint32(13,true);
    const fullW=dv.getUint32(5,true), fullH=dv.getUint32(9,true);
    const patch=(flags&4)!==0, cursor=(flags&2)!==0;
    const cx=dv.getInt16(19,true), cy=dv.getInt16(21,true), cw=dv.getUint16(23,true), ch=dv.getUint16(25,true);
    let x0=0, y0=0, w=fullW, h=fullH, start=27, format=0;
    if(patch){
      if(size<20)return;
      x0=dv.getUint32(27,true); y0=dv.getUint32(31,true);
      w=dv.getUint32(35,true); h=dv.getUint32(39,true); format=dv.getUint32(43,true);
      start=47;
    }
    const bytes=b.slice(start,27+size);
    chain=chain.then(()=>draw({fullW,fullH,x:x0,y:y0,w,h,format,cursor,cx,cy,cw,ch,bytes,patch})).catch(()=>{});
  };
  s.onclose=()=>{stopAudio(false);pending.forEach(cb=>cb({op:'error.err',payload:{message:'连接断开'}}));pending.clear();stream=null;bar.textContent=`${id}: 断线,1s 后重连`;setTimeout(stream2,1000);};
}
fetch('/api/windows').then(r=>r.json()).then(d=>{
  const w=(d.windows||[]).find(w=>w.id===id);
  title(w?(w.title||w.appId||`窗口 ${id}`):`窗口 ${id}`);
  if(w)setMode(w.mode||'free');
}).catch(()=>title(`窗口 ${id}`));
setInterval(pollMode,2000);
stream2();
</script>"#;

enum Route {
    Index,
    View(u32),
    Windows,
    Stream(u32),
}

/// 解析请求路径(?query 剥离);未知路径回 None → 404。
fn route(path: &str) -> Option<Route> {
    let path = path.split('?').next().unwrap_or(path);
    match path {
        "/" | "/index.html" => Some(Route::Index),
        "/api/windows" => Some(Route::Windows),
        _ => {
            if let Some(id) = path.strip_prefix("/api/stream/") {
                id.parse().ok().map(Route::Stream)
            } else if let Some(id) = path.strip_prefix("/view/") {
                id.parse().ok().map(Route::View)
            } else {
                None
            }
        }
    }
}

fn http(body: &[u8], content_type: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(body.iter().copied())
    .collect()
}

fn http_404() -> Vec<u8> {
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// 浏览器发起的 WS 必须与页面同源，避免任意网页连到本地视图服务注入输入。
/// 非浏览器客户端（e2e/CLI）通常不带 Origin，保持兼容。
fn websocket_origin_allowed(head: &str) -> bool {
    let Some(origin) = header_value(head, "origin") else {
        return true;
    };
    let Some(host) = header_value(head, "host") else {
        return false;
    };
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(|origin_host| origin_host.eq_ignore_ascii_case(host))
}

pub async fn serve(client: Client, bind: &str) -> Result<(), Error> {
    let listener = TcpListener::bind(bind).await?;
    println!("本地视图: http://{bind}(每个容器窗口一个独立视图页)");
    loop {
        let (mut stream, _) = listener.accept().await?;
        let client = client.clone();
        tokio::spawn(async move {
            let mut probe = [0u8; 1024];
            let peeked = stream.peek(&mut probe).await.unwrap_or(0);
            let request_head = String::from_utf8_lossy(&probe[..peeked]);
            let path = request_head
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_owned();
            if request_head.contains("Upgrade: websocket")
                || request_head.contains("upgrade: websocket")
            {
                if !websocket_origin_allowed(&request_head) {
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }
                if let Some(Route::Stream(window)) = route(&path) {
                    stream_window(client, stream, window).await;
                }
                return;
            }
            let mut request = [0u8; 4096];
            if stream.read(&mut request).await.is_err() {
                return;
            }
            let response = match route(&path) {
                Some(Route::Index) => http(INDEX_HTML.as_bytes(), "text/html; charset=utf-8"),
                Some(Route::View(id)) => {
                    let body = VIEW_HTML.replace("__ID__", &id.to_string());
                    http(body.as_bytes(), "text/html; charset=utf-8")
                }
                Some(Route::Windows) => {
                    match client.call("display.list", serde_json::json!({})).await {
                        Ok(e) => http(
                            e.payload
                                .unwrap_or_else(|| serde_json::json!({"windows": []}))
                                .to_string()
                                .as_bytes(),
                            "application/json",
                        ),
                        Err(_) => http(br#"{"windows":[]}"#, "application/json"),
                    }
                }
                _ => http_404(),
            };
            let _ = stream.write_all(&response).await;
        });
    }
}

/// 流 WS 允许转发的控制 op(白名单:视图页只需要输入与接管;服务端持完整
/// 客户端凭据,快照等高权限 op 不放行)。
fn control_allowed(op: &str) -> bool {
    matches!(
        op,
        "display.input" | "display.takeover" | "display.list" | "audio.attach" | "audio.detach"
    )
}

/// 解析视图页控制信封 → (id, op, payload);缺 id/op 视为非控制信封。
fn parse_control(text: &str) -> Option<(String, String, serde_json::Value)> {
    let env: serde_json::Value = serde_json::from_str(text).ok()?;
    let id = env.get("id")?.as_str()?.to_owned();
    let op = env.get("op")?.as_str()?.to_owned();
    let payload = env
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Some((id, op, payload))
}

/// 单窗口帧流:一次 WS 连接对应一次 attach,页面断开即 detach
/// (损伤门控下静态窗口零流量,attach 时服务端强制出首帧)。
/// 下行二进制帧(png,damage 补丁);上行仅白名单控制信封(输入/接管/列表)。
async fn stream_window(client: Client, stream: tokio::net::TcpStream, window: u32) {
    let Ok(socket) = accept_async(stream).await else {
        return;
    };
    let (mut sink, mut source) = socket.split();
    let _ = client
        .call(
            "display.attach",
            serde_json::json!({ "window": window, "fps": 30, "codec": "png" }),
        )
        .await;
    let (bin_tx, mut bin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let mut frames = client.subscribe_frames();
    let control = client.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                incoming = source.next() => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let Some((id, op, payload)) = parse_control(&text) else {
                            continue;
                        };
                        if !control_allowed(&op) {
                            continue;
                        }
                        let reply = match control.call(&op, payload).await {
                            Ok(e) => serde_json::json!({
                                "v": 1, "id": mixum_protocol::envelope::new_id(),
                                "op": e.op, "replyTo": id, "payload": e.payload,
                            }),
                            Err(e) => serde_json::json!({
                                "v": 1, "id": mixum_protocol::envelope::new_id(),
                                "op": "error.err", "replyTo": id,
                                "payload": {"code": "internal", "message": e.to_string()},
                            }),
                        };
                        if sink.send(Message::Text(reply.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                },
                data = bin_rx.recv() => match data {
                    Some(data) => {
                        if sink.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    });
    while let Ok(frame) = frames.recv().await {
        // 音频是容器级帧（0x02），不按窗口过滤；video 帧只转发本窗口。
        let forward = if frame.0.first() == Some(&mixum_protocol::frames::chan::AUDIO) {
            true
        } else {
            unmarshal_video(&frame.0)
                .map(|video| video.window_id == window)
                .unwrap_or(false)
        };
        if forward && bin_tx.send(frame.0).await.is_err() {
            break;
        }
    }
    let _ = client
        .call("display.detach", serde_json::json!({ "window": window }))
        .await;
    let _ = client.call("audio.detach", serde_json::json!({})).await;
    writer.abort();
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

#[cfg(test)]
mod tests {
    use super::{control_allowed, parse_control, route, websocket_origin_allowed, Route};

    #[test]
    fn routes_cover_viewer_pages_and_strip_query() {
        assert!(matches!(route("/"), Some(Route::Index)));
        assert!(matches!(route("/index.html"), Some(Route::Index)));
        assert!(matches!(route("/api/windows?all=1"), Some(Route::Windows)));
        assert!(matches!(route("/view/7"), Some(Route::View(7))));
        assert!(matches!(route("/api/stream/12"), Some(Route::Stream(12))));
        assert!(route("/api/stream/").is_none());
        assert!(route("/view/abc").is_none());
        assert!(route("/etc/passwd").is_none());
    }

    #[test]
    fn control_channel_is_whitelisted_and_envelopes_strict() {
        assert!(control_allowed("display.input"));
        assert!(control_allowed("audio.attach"));
        assert!(control_allowed("audio.detach"));
        assert!(control_allowed("display.takeover"));
        assert!(control_allowed("display.list"));
        // 服务端持完整凭据,高权限 op 绝不经视图 WS 放行。
        assert!(!control_allowed("snap.restore"));
        assert!(!control_allowed("task.submit"));

        let (id, op, payload) =
            parse_control(r#"{"v":1,"id":"abc","op":"display.input","payload":{"x":1}}"#)
                .expect("完整信封应解析");
        assert_eq!((id.as_str(), op.as_str()), ("abc", "display.input"));
        assert_eq!(payload["x"], 1);
        let (id, op, payload) = parse_control(r#"{"v":1,"id":"abc","op":"display.takeover"}"#)
            .expect("无 payload 应默认 Null");
        assert_eq!((id.as_str(), op.as_str()), ("abc", "display.takeover"));
        assert!(payload.is_null());
        assert!(
            parse_control(r#"{"v":1,"op":"display.input"}"#).is_none(),
            "缺 id 拒绝"
        );
        assert!(parse_control("not json").is_none());
    }

    #[test]
    fn websocket_origin_must_match_host() {
        let same = "GET /api/stream/1 HTTP/1.1\r\nHost: 127.0.0.1:7602\r\nOrigin: http://127.0.0.1:7602\r\nUpgrade: websocket\r\n";
        assert!(websocket_origin_allowed(same));
        let evil = "GET /api/stream/1 HTTP/1.1\r\nHost: 127.0.0.1:7602\r\nOrigin: http://evil.example\r\nUpgrade: websocket\r\n";
        assert!(!websocket_origin_allowed(evil));
        let no_origin =
            "GET /api/stream/1 HTTP/1.1\r\nHost: 127.0.0.1:7602\r\nUpgrade: websocket\r\n";
        assert!(websocket_origin_allowed(no_origin));
        let null_origin =
            "GET /api/stream/1 HTTP/1.1\r\nHost: 127.0.0.1:7602\r\nOrigin: null\r\nUpgrade: websocket\r\n";
        assert!(!websocket_origin_allowed(null_origin));
    }
}
