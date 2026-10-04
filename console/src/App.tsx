import { useEffect } from 'react'
import { Header } from './components/Header'
import { Settings } from './components/Settings'
import { TasksView } from './components/TasksView'
import { WindowsView } from './components/WindowsView'
import { SnapshotsView } from './components/SnapshotsView'
import { useStore, type NarrowView } from './state/store'

const TABS: { key: NarrowView; label: string }[] = [
  { key: 'tasks', label: '任务' },
  { key: 'windows', label: '窗口' },
  { key: 'snaps', label: '快照' },
]

export default function App() {
  const narrowView = useStore((s) => s.narrowView)
  const setNarrowView = useStore((s) => s.setNarrowView)
  const settingsOpen = useStore((s) => s.settingsOpen)
  const connect = useStore((s) => s.connect)
  const token = useStore((s) => s.token)

  // 已有令牌则启动即连(令牌与地址持久化在 localStorage)
  useEffect(() => {
    if (token.trim()) connect()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  return (
    <div className="app">
      <Header />
      <main className={`columns view-${narrowView}`}>
        <TasksView />
        <WindowsView />
        <SnapshotsView />
      </main>
      <nav className="tabs">
        {TABS.map((t) => (
          <button
            key={t.key}
            className={`tab ${narrowView === t.key ? 'active' : ''}`}
            onClick={() => setNarrowView(t.key)}
          >
            {t.label}
          </button>
        ))}
      </nav>
      <Toast />
      {settingsOpen && <Settings />}
    </div>
  )
}

function Toast() {
  const toast = useStore((s) => s.toast)
  if (!toast) return null
  return <div className={`toast ${toast.kind === 'error' ? 'error' : ''}`}>{toast.text}</div>
}
