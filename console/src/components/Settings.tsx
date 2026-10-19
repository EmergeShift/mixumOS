import { useEffect, useRef, useState } from 'react'
import { client, msg, refreshDevices, selectNode, useStore } from '../state/store'

export function Settings() {
  const address = useStore((s) => s.address)
  const token = useStore((s) => s.token)
  const device = useStore((s) => s.device)
  const status = useStore((s) => s.status)
  const nodes = useStore((s) => s.nodes)
  const selectedNode = useStore((s) => s.selectedNode)
  const devices = useStore((s) => s.devices)
  const setSetting = useStore((s) => s.setSetting)
  const connect = useStore((s) => s.connect)
  const disconnect = useStore((s) => s.disconnect)
  const setSettingsOpen = useStore((s) => s.setSettingsOpen)
  const showToast = useStore((s) => s.showToast)

  const addrRef = useRef<HTMLInputElement>(null)
  // gateway 配对/设备管理(仅已连接且服务端为 gateway 时展示)
  const isGateway = client.isGateway() && status === 'connected'
  useEffect(() => addrRef.current?.focus(), [])
  useEffect(() => {
    if (isGateway) void refreshDevices()
  }, [isGateway])

  const [pairCodeLocal, setPairCodeLocal] = useState<string | null>(null)
  const [claimCode, setClaimCode] = useState('')
  const [claimName, setClaimName] = useState('')

  async function createPair() {
    try {
      const r = await client.pairCreate()
      setPairCodeLocal(r.code)
    } catch (err) {
      showToast(`配对码生成失败:${msg(err)}`, 'error')
    }
  }

  async function claimPair() {
    if (!claimCode.trim() || !claimName.trim()) return
    try {
      const r = await client.pairClaim(claimCode.trim(), claimName.trim())
      setSetting('token', r.token)
      showToast(`设备已接入:${r.deviceId}`)
      setClaimCode('')
      setClaimName('')
      setPairCodeLocal(null)
      void refreshDevices()
    } catch (err) {
      showToast(`配对失败:${msg(err)}`, 'error')
    }
  }

  async function revokeDevice(jti: string) {
    try {
      await client.deviceRevoke(jti)
      showToast('设备已吊销')
      void refreshDevices()
    } catch (err) {
      showToast(`吊销失败:${msg(err)}`, 'error')
    }
  }

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
        {isGateway && nodes.length > 0 && (
          <label className="field">
            <span>节点</span>
            <select
              value={selectedNode}
              onChange={(e) => selectNode(e.target.value)}
            >
              {nodes.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
        )}
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

        {isGateway && (
          <div className="gateway-devices">
            <h3>设备管理</h3>
            <div className="form-row">
              <button className="btn btn-small" onClick={() => void createPair()}>
                生成配对码
              </button>
              {pairCodeLocal && (
                <span className="mono pair-code">配对码:{pairCodeLocal}(5 分钟内有效)</span>
              )}
            </div>
            <div className="form-row params">
              <input
                value={claimCode}
                onChange={(e) => setClaimCode(e.target.value)}
                placeholder="配对码"
                spellCheck={false}
              />
              <input
                value={claimName}
                onChange={(e) => setClaimName(e.target.value)}
                placeholder="设备名"
              />
              <button
                className="btn btn-small"
                onClick={() => void claimPair()}
                disabled={!claimCode.trim() || !claimName.trim()}
              >
                接入
              </button>
            </div>
            <ul className="device-list">
              {devices.map((d) => (
                <li key={d.jti} className="device-item">
                  <span>{d.name}</span>
                  <span className="mono">{d.sub}</span>
                  <button
                    className="btn btn-small"
                    onClick={() => void revokeDevice(d.jti)}
                  >
                    吊销
                  </button>
                </li>
              ))}
              {devices.length === 0 && <li className="empty">暂无已配对设备</li>}
            </ul>
            <button className="btn btn-small" onClick={() => void refreshDevices()}>
              刷新设备
            </button>
          </div>
        )}
      </div>
    </div>
  )
}
