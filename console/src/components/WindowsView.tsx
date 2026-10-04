import { useEffect } from 'react'
import { useStore } from '../state/store'
import { WindowCanvas } from './WindowCanvas'

export function WindowsView() {
  const windows = useStore((s) => s.windows)
  const activeWindow = useStore((s) => s.activeWindow)
  const openWindow = useStore((s) => s.openWindow)
  const refreshWindows = useStore((s) => s.refreshWindows)

  // 打开视图时刷一次列表;平时靠 display.windows 推送
  useEffect(() => {
    void refreshWindows()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const active = windows.find((w) => w.id === activeWindow)

  return (
    <section className="col col-windows">
      <div className="col-head">
        窗口
        {windows.length > 0 && <span className="col-head-meta">{windows.length}</span>}
      </div>
      <ul className="win-list">
        {windows.map((w) => (
          <li
            key={w.id}
            className={`win-item ${activeWindow === w.id ? 'selected' : ''}`}
            onClick={() => openWindow(w.id)}
          >
            <span className={`mode-badge mode-${w.mode}`}>
              {w.mode === 'agent' ? 'agent' : w.mode === 'user' ? 'user' : 'free'}
            </span>
            <span className="win-title" title={w.title}>
              {w.title}
            </span>
            <span className="win-meta">
              {w.width}×{w.height} · {w.appId}
            </span>
          </li>
        ))}
        {windows.length === 0 && <li className="empty">暂无窗口(compositor 未就绪或没有打开的应用)</li>}
      </ul>
      {active ? (
        <WindowCanvas win={active} />
      ) : (
        <div className="canvas-empty">从上方选择一个窗口附流观看</div>
      )}
    </section>
  )
}
