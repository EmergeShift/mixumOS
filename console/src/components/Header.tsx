import { useStore } from '../state/store'

const STATUS_TEXT: Record<string, string> = {
  connected: '已连接',
  connecting: '连接中',
  disconnected: '未连接',
}

export function Header() {
  const status = useStore((s) => s.status)
  const statusDetail = useStore((s) => s.statusDetail)
  const hello = useStore((s) => s.hello)
  const setSettingsOpen = useStore((s) => s.setSettingsOpen)

  return (
    <header className="topbar">
      <div className="brand">mixumOS 控制台</div>
      <div className={`conn status-${status}`} title={statusDetail}>
        <span className="dot" />
        {STATUS_TEXT[status]}
        {statusDetail ? ` · ${statusDetail}` : ''}
      </div>
      {hello && (
        <div className="server-info" title={hello.sessionId}>
          {hello.server} · {hello.arch}
        </div>
      )}
      <button className="btn" onClick={() => setSettingsOpen(true)}>
        设置
      </button>
    </header>
  )
}
