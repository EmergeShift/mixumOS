import { useEffect, useMemo, useRef, useState } from 'react'
import { client, msg, useStore } from '../state/store'
import type { SnapshotPolicy, TaskState } from '../protocol/types'
import { fmtDateTime, fmtTime, shortId } from '../util/format'

const STATE_LABEL: Record<TaskState, string> = {
  queued: '排队',
  running: '运行中',
  suspended: '挂起',
  completed: '已完成',
  failed: '失败',
  canceled: '已取消',
}

interface ParamRow {
  key: string
  value: string
}

export function TasksView() {
  const status = useStore((s) => s.status)
  const tasks = useStore((s) => s.tasks)
  const selectedTask = useStore((s) => s.selectedTask)
  const selectTask = useStore((s) => s.selectTask)
  const refreshTasks = useStore((s) => s.refreshTasks)
  const showToast = useStore((s) => s.showToast)
  const setNarrowView = useStore((s) => s.setNarrowView)

  const [instruction, setInstruction] = useState('')
  const [agent, setAgent] = useState('shell')
  const [params, setParams] = useState<ParamRow[]>([{ key: 'cmd', value: '' }])
  const [preSnap, setPreSnap] = useState(true)
  const [kind, setKind] = useState<'disk' | 'memory' | 'hybrid'>('disk')
  const [onFailure, setOnFailure] = useState<'none' | 'restore'>('none')
  const [submitting, setSubmitting] = useState(false)

  const taskList = useMemo(
    () => Object.values(tasks).sort((a, b) => b.createdAt - a.createdAt),
    [tasks]
  )

  async function submit() {
    if (!instruction.trim() || submitting) return
    setSubmitting(true)
    const snapshotPolicy: SnapshotPolicy = { pre: preSnap, kind, onFailure }
    try {
      const kv: Record<string, string> = {}
      for (const r of params) if (r.key.trim()) kv[r.key.trim()] = r.value
      const t = await client.submitTask({
        instruction: instruction.trim(),
        agent: agent.trim() || undefined,
        params: Object.keys(kv).length ? kv : undefined,
        snapshotPolicy,
      })
      showToast(`任务已提交:${shortId(t.id)}`)
      setInstruction('')
      setParams([{ key: 'cmd', value: '' }])
      selectTask(t.id)
      void refreshTasks()
    } catch (err) {
      showToast(`提交失败:${msg(err)}`, 'error')
    } finally {
      setSubmitting(false)
    }
  }

  async function cancel(id: string) {
    try {
      await client.cancelTask(id)
      showToast('已请求取消')
    } catch (err) {
      showToast(`取消失败:${msg(err)}`, 'error')
    }
  }

  return (
    <section className="col col-tasks">
      <div className="col-head">任务</div>
      <div className="col-body">
        <form
          className="task-form"
          onSubmit={(e) => {
            e.preventDefault()
            void submit()
          }}
        >
          <textarea
            className="instruction"
            value={instruction}
            onChange={(e) => setInstruction(e.target.value)}
            placeholder="给 agent 的指令,如:整理 ~/downloads 里的文件"
            rows={2}
          />
          <div className="form-row">
            <label className="field">
              <span>agent</span>
              <input value={agent} onChange={(e) => setAgent(e.target.value)} list="agent-list" />
              <datalist id="agent-list">
                <option value="shell" />
              </datalist>
            </label>
          </div>
          <div className="form-row params">
            <span className="field-label">参数</span>
            <div className="param-rows">
              {params.map((p, i) => (
                <div className="param-row" key={i}>
                  <input
                    value={p.key}
                    onChange={(e) =>
                      setParams(params.map((r, j) => (j === i ? { ...r, key: e.target.value } : r)))
                    }
                    placeholder="键"
                    spellCheck={false}
                  />
                  <input
                    value={p.value}
                    onChange={(e) =>
                      setParams(params.map((r, j) => (j === i ? { ...r, value: e.target.value } : r)))
                    }
                    placeholder="值"
                    spellCheck={false}
                  />
                  <button
                    type="button"
                    className="btn btn-icon"
                    onClick={() => setParams(params.filter((_, j) => j !== i))}
                    aria-label="删除参数"
                  >
                    ×
                  </button>
                </div>
              ))}
              <button
                type="button"
                className="btn btn-small"
                onClick={() => setParams([...params, { key: '', value: '' }])}
              >
                添加参数
              </button>
            </div>
          </div>
          <div className="form-row snap-policy">
            <label className="check">
              <input
                type="checkbox"
                checked={preSnap}
                onChange={(e) => setPreSnap(e.target.checked)}
              />
              前置快照
            </label>
            <select value={kind} onChange={(e) => setKind(e.target.value as typeof kind)} disabled={!preSnap}>
              <option value="disk">disk</option>
              <option value="memory">memory</option>
              <option value="hybrid">hybrid</option>
            </select>
            <select
              value={onFailure}
              onChange={(e) => setOnFailure(e.target.value as typeof onFailure)}
              disabled={!preSnap}
            >
              <option value="none">失败不回退</option>
              <option value="restore">失败自动回退</option>
            </select>
          </div>
          <button
            className="btn btn-primary btn-block"
            type="submit"
            disabled={status !== 'connected' || submitting}
          >
            {submitting ? '提交中…' : '提交任务'}
          </button>
        </form>

        <ul className="task-list">
          {taskList.map((t) => (
            <li
              key={t.id}
              className={`task-item state-${t.state} ${selectedTask === t.id ? 'selected' : ''}`}
              onClick={() => selectTask(t.id)}
            >
              <div className="task-line">
                <span className="state-badge">{STATE_LABEL[t.state]}</span>
                <span className="task-instruction">{t.instruction}</span>
              </div>
              <div className="task-meta">
                <span>{t.agent}</span>
                <span>{fmtDateTime(t.createdAt)}</span>
                <span>{shortId(t.id)}</span>
                {(t.state === 'queued' || t.state === 'running') && (
                  <button
                    className="btn btn-small"
                    onClick={(e) => {
                      e.stopPropagation()
                      void cancel(t.id)
                    }}
                  >
                    取消
                  </button>
                )}
              </div>
            </li>
          ))}
          {taskList.length === 0 && <li className="empty">暂无任务</li>}
        </ul>
      </div>
      {selectedTask && <EventStream onJumpWindows={() => setNarrowView('windows')} />}
    </section>
  )
}

function EventStream({ onJumpWindows }: { onJumpWindows: () => void }) {
  const events = useStore((s) => s.events)
  const selectedTask = useStore((s) => s.selectedTask)
  const tasks = useStore((s) => s.tasks)
  const selectTask = useStore((s) => s.selectTask)
  const bodyRef = useRef<HTMLDivElement>(null)

  const list = useMemo(
    () => events.filter((e) => e.taskId === selectedTask),
    [events, selectedTask]
  )

  useEffect(() => {
    const el = bodyRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [list.length])

  const task = selectedTask ? tasks[selectedTask] : undefined

  return (
    <div className="event-panel">
      <div className="event-head">
        <span>事件流 · {task ? task.instruction : shortId(selectedTask ?? '')}</span>
        <button className="btn btn-small" onClick={() => selectTask(null)}>
          收起
        </button>
      </div>
      <div className="event-body" ref={bodyRef}>
        {list.map((e, i) => (
          <div className={`event ev-${e.type}`} key={i}>
            <span className="ev-ts">{fmtTime(e.ts)}</span>
            <span className="ev-type">{e.step ? `${e.type}/${e.step}` : e.type}</span>
            <span className="ev-data">
              {e.type === 'log' && typeof e.data === 'string' && e.data}
              {e.type === 'state' && String(e.data)}
              {e.type === 'progress' && e.data != null && JSON.stringify(e.data)}
              {e.type === 'error' && String(e.data)}
              {e.type === 'window' && (
                <>
                  {JSON.stringify(e.data)}{' '}
                  <button className="btn btn-small" onClick={onJumpWindows}>
                    打开窗口画布
                  </button>
                </>
              )}
            </span>
          </div>
        ))}
        {list.length === 0 && <div className="empty">订阅后等待事件…</div>}
      </div>
    </div>
  )
}
