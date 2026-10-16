import { useEffect, useRef } from 'react'
import { useStore } from '../state/store'

export function Settings() {
  const address = useStore((s) => s.address)
  const token = useStore((s) => s.token)
  const device = useStore((s) => s.device)
  const status = useStore((s) => s.status)
  const setSetting = useStore((s) => s.setSetting)
  const connect = useStore((s) => s.connect)
  const disconnect = useStore((s) => s.disconnect)
  const setSettingsOpen = useStore((s) => s.setSettingsOpen)

  // 打开时聚焦地址框
  const addrRef = useRef<HTMLInputElement>(null)
  useEffect(() => addrRef.current?.focus(), [])

  return (
    <div className="overlay" onClick={() => setSettingsOpen(false)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>连接设置</h2>
        <label className="field">
          <span>服务地址</span>
          <input
            ref={addrRef}
            value={address}
            onChange={(e) => setSetting('address', e.target.value)}
            placeholder="wss://127.0.0.1:17601 或 /ws(走开发中继)"
            spellCheck={false}
          />
        </label>
        <label className="field">
          <span>能力令牌</span>
          <textarea
            value={token}
            onChange={(e) => setSetting('token', e.target.value)}
            rows={4}
            placeholder="mixum1.…(容器日志 READY 行)"
            spellCheck={false}
          />
        </label>
        <label className="field">
          <span>设备名</span>
          <input value={device} onChange={(e) => setSetting('device', e.target.value)} />
        </label>
        <p className="hint">
          开发模式(npm run dev)默认走 /ws,由本机 vite 中继转发,不校验自签证书,
          仅限本机开发。静态部署后需直连 wss:// 地址,并先信任服务端证书,
          见 console/README.md。
        </p>
        <div className="modal-actions">
          <button className="btn" onClick={disconnect} disabled={status === 'disconnected'}>
            断开
          </button>
          <button
            className="btn btn-primary"
            onClick={() => {
              connect()
              setSettingsOpen(false)
            }}
            disabled={!token.trim() || !address.trim()}
          >
            保存并连接
          </button>
          <button className="btn" onClick={() => setSettingsOpen(false)}>
            关闭
          </button>
        </div>
      </div>
    </div>
  )
}
